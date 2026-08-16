//! Multi-stream / layer-graph e2e test (issue #394) — ports the ad-hoc, repo-external
//! `multistream_test.py` harness into a first-class, always-on integration test.
//!
//! Drives a real `liminis-context-graph --mcp-stdio` process through nine phases using only the
//! assertion API (`knowledge_assert_entity`/`knowledge_assert_relationship`/
//! `knowledge_add_cross_group_edge` — FR-002: no episode ingest, no extraction, no corpus
//! fixture, no LLM) across three groups: `A` and `B` as independent source graphs, `C` as a
//! layer graph holding only cross-group edges into them. Assertions check both MCP responses and
//! the on-disk WAL layout (FR-003) — the latter is what caught #385, since a purely
//! response-shape check can't see which group's `.jsonl` stream a mutation actually landed in.
//!
//! This test exists to protect the *composition* of several independently-shipped features —
//! #361 (purge), #365 (checkpoints), #369 (pointers), #378 (per-group streams), #383
//! (positions), #385 (mutation attribution), #387 (generations) — because the three real defects
//! that motivated this issue (#383, #385, #392) each lived in that composition, invisible to any
//! single-feature test. Every assertion below names the issue it protects (FR-005).
//!
//! Phase 9's assertion covers #392: a staleness gate in
//! `crates/core/src/cross_group.rs::rebind_pointers_impl` used to skip re-checking any pointer
//! whose `bound_at_seq` looked fresh, even when its recorded `binding_state` was `unbound` —
//! reachable through purge (#361) followed by a checkpoint-bounded restore (#365). The gate now
//! also keys on binding state, so this assertion requires every pointer into A be `bound` after
//! the public `knowledge_rebind_pointers` call.
//!
//! Issue #400 adds four more phases that compose the same real-process/on-disk-WAL harness with
//! behaviors that had first coverage only at the `crates/core` integration level, never at this
//! process boundary. They land in two places, not all in one, because two of the four raw-
//! manipulate group A's on-disk WAL directory directly (bypassing the live `WalWriter`), and
//! phase 7's purge is itself a live write into A's own stream (`knowledge_delete_by_group`
//! flushes each purged group's own deletions to its own writer, issue #385/ADR-0385) — letting
//! that live write land *after* a raw manipulation, ahead of phase 8's checkpoint-bounded
//! from-scratch replay, produced an unrecoverable hang in the spawned process during development
//! of this issue. So the two raw-manipulation phases run only after phase 9, the last point at
//! which no further live write to A ever occurs — matching the safety invariant
//! `crates/core/tests/wal_generation_reset.rs` already relies on for the same kind of raw
//! manipulation one layer down:
//!
//! - **Cross-group merge** (#368/#371, ADR-0371) — inserted between the original phase 4 and
//!   phase 5: two entities in A are merged, live over MCP (no raw manipulation), while C holds a
//!   cross-group edge into the one that loses. Placed *before* the pre-existing purge sequence
//!   (phases 6-9), per the spec's ordering constraint, so C's pointer is still `bound` when the
//!   merge runs. Asserts the merge's mutations land only in A's own WAL stream — never B's or
//!   C's — and that C's pointer follows `merged_into` forwarding to the survivor.
//! - **Ambiguous resolution** (#369, ADR-0369) — after phase 9: two active entities in A share
//!   one name that a pointer from C targets — constructed by raw-injecting WAL lines directly,
//!   since `knowledge_assert_entity`'s `(name, group_id)` idempotency can never produce this
//!   state. Asserts the pointer's `binding_state` is `ambiguous`, never a silently picked
//!   winner, and that `knowledge_rebind_pointers` re-examines it rather than skipping it as
//!   already resolved.
//! - **Generation reset** (#387, ADR-0387) — after the ambiguous-resolution phase: group A's
//!   entire stream is raw-republished under a new `.wal-generation.json` identity with entirely
//!   new entity identities while C holds pointers into it. Asserts the reset self-heal path
//!   re-binds C's pointers to A's new entities by name, and that group B is untouched. Its
//!   wipe-everything semantics also cleanly subsumes the ambiguous-resolution phase's scratch
//!   entities.
//! - **Missing generation sidecar** (#414) — last, before shutdown: B's `.wal-generation.json`
//!   is deleted while its `.jsonl` content is left intact — the shape a mis-published stream has
//!   in practice (#414's confirmed root cause was a `*.jsonl`-only glob silently dropping the
//!   dot-namespaced sidecar). Asserts `knowledge_status` reports the condition distinguishably
//!   (FR-001) and that a subsequent `knowledge_rebuild_from_wal` against B refuses outright
//!   rather than silently proceeding, since B already has a recorded position (FR-002).

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

mod common;
use common::wal_inspect::wal_snapshot;
use common::{binary_path, spawn_stub_embedder, wal_manipulate, McpClient};

fn structured<'a>(resp: &'a Value, context: &str) -> &'a Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    &resp["result"]["structuredContent"]
}

fn assert_entity(client: &mut McpClient, group: &str, name: &str) -> String {
    let resp = client.call_tool(
        "knowledge_assert_entity",
        json!({"group_id": group, "name": name, "labels": ["Entity"]}),
    );
    let result = structured(&resp, &format!("assert_entity {group}/{name}"));
    result["entity_uuid"]
        .as_str()
        .unwrap_or_else(|| panic!("assert_entity {group}/{name} returned no entity_uuid: {result}"))
        .to_string()
}

fn assert_relationship(
    client: &mut McpClient,
    group: &str,
    source: &str,
    predicate: &str,
    target: &str,
) {
    let resp = client.call_tool(
        "knowledge_assert_relationship",
        json!({
            "group_id": group,
            "source_name": source,
            "predicate": predicate,
            "target_name": target,
        }),
    );
    structured(
        &resp,
        &format!("assert_relationship {group}: {source} -[{predicate}]-> {target}"),
    );
}

fn cross_group_edge(client: &mut McpClient, group: &str, name: &str, source: Value, target: Value) {
    let resp = client.call_tool(
        "knowledge_add_cross_group_edge",
        json!({
            "group_id": group,
            "name": name,
            "fact": format!("{name} asserted by the {group} layer"),
            "source": source,
            "target": target,
        }),
    );
    structured(&resp, &format!("add_cross_group_edge {group}/{name}"));
}

fn wal_groups(client: &mut McpClient) -> Value {
    let resp = client.call_tool("knowledge_status", json!({}));
    structured(&resp, "knowledge_status")["wal_groups"].clone()
}

/// One cross-group pointer's resolution target and current state, read off a `RelatesToNode_`
/// row's `attributes.cross_group_pointers.<side>` JSON.
#[derive(Debug, Clone)]
struct PointerInfo {
    source_group_id: String,
    binding_state: String,
    /// The endpoint name the pointer targets — needed to distinguish which pointer is which
    /// once a phase creates more than one pointer into the same group (#400 FR-002/FR-003).
    endpoint_name: String,
    /// The uuid the pointer currently resolves to, when `binding_state == "bound"` — needed to
    /// assert *which* entity a pointer follows a reset (#387) or a merge's `merged_into`
    /// forwarding (#368/#371) to, not just that it is bound to *something*.
    resolved_uuid: Option<String>,
}

/// Reads group `C`'s `RelatesToNode_` rows and returns, per edge name, the [`PointerInfo`] of
/// each cross-group pointer side (`"src"`/`"dst"`) it carries — the same shape the reference
/// Python harness's `c_bindings()` produces, plus `source_group_id` so callers can distinguish
/// "a pointer into A" from "a pointer into B" rather than treating every pointer alike.
/// `handle_query_cypher` returns `rows` as `Vec<Vec<String>>` (stringified columns, not maps —
/// confirmed against `crates/core/src/handlers.rs`), so each row is indexed positionally: `[0]`
/// is `rn.name`, `[1]` is `rn.attributes` (a JSON string).
fn c_bindings(client: &mut McpClient) -> HashMap<String, HashMap<String, PointerInfo>> {
    let query = "MATCH (rn:RelatesToNode_) WHERE rn.group_id = 'C' RETURN rn.name, rn.attributes";
    let resp = client.call_tool("knowledge_query_cypher", json!({"query": query}));
    let result = structured(&resp, "query_cypher (c_bindings)");

    let mut out = HashMap::new();
    for row in result["rows"].as_array().into_iter().flatten() {
        let cols = row.as_array().cloned().unwrap_or_default();
        let name = cols
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let attrs_str = cols.get(1).and_then(|v| v.as_str()).unwrap_or("{}");
        let attrs: Value = serde_json::from_str(attrs_str).unwrap_or_default();
        let pointers = attrs
            .get("cross_group_pointers")
            .cloned()
            .unwrap_or_default();

        let mut sides = HashMap::new();
        if let Some(obj) = pointers.as_object() {
            for (side, ptr) in obj {
                sides.insert(
                    side.clone(),
                    PointerInfo {
                        source_group_id: ptr["source_group_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        binding_state: ptr["binding_state"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        endpoint_name: ptr["endpoint_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        resolved_uuid: ptr["resolved_uuid"].as_str().map(str::to_string),
                    },
                );
            }
        }
        out.insert(name, sides);
    }
    out
}

/// The `"dst"`-side [`PointerInfo`] of the named edge — every cross-group pointer this test
/// constructs (existing and new) puts its foreign endpoint on `target`/`"dst"` (`source` is
/// always `{"uuid": ...}`, already local to the edge's own group), so this is the one accessor
/// every new phase's by-uuid assertions need.
fn dst_pointer<'a>(
    bindings: &'a HashMap<String, HashMap<String, PointerInfo>>,
    edge_name: &str,
) -> &'a PointerInfo {
    bindings
        .get(edge_name)
        .and_then(|sides| sides.get("dst"))
        .unwrap_or_else(|| {
            panic!("expected a dst-side pointer on edge {edge_name:?}: {bindings:?}")
        })
}

/// The `binding_state` of every cross-group pointer (across all of C's edges, either side) that
/// resolves into `into_group` specifically — e.g. `binding_states_into(&bindings, "A")` isolates
/// pointers into A from pointers into B, so a regression that unbinds the wrong group's pointers
/// still gets caught instead of passing a weaker "some pointer somewhere is unbound" check.
fn binding_states_into<'a>(
    bindings: &'a HashMap<String, HashMap<String, PointerInfo>>,
    into_group: &str,
) -> Vec<&'a str> {
    bindings
        .values()
        .flat_map(|sides| sides.values())
        .filter(|p| p.source_group_id == into_group)
        .map(|p| p.binding_state.as_str())
        .collect()
}

#[test]
fn multistream_layer_graph_composition() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("test.db");
    let wal_dir = tmp.path().join("wal");

    let port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .args(["--mcp-stdio", "--embedder-http", &embedder_url])
        // Deliberately dead port: FR-002 means no extraction is ever attempted in this test.
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"]);
    let mut client = McpClient::spawn(cmd);
    client.initialize();

    // ── Phase 1: independent groups A and B, entities + an intra-group relationship each ──
    assert_entity(&mut client, "A", "A1");
    assert_entity(&mut client, "A", "A2");
    assert_relationship(&mut client, "A", "A1", "REL_A", "A2");
    assert_entity(&mut client, "B", "B1");
    assert_entity(&mut client, "B", "B2");
    assert_relationship(&mut client, "B", "B1", "REL_B", "B2");

    // ── Phase 2: a third group C, created lazily on first write ─────────────────────────────
    let c1 = assert_entity(&mut client, "C", "C1");
    let c2 = assert_entity(&mut client, "C", "C2");
    assert_relationship(&mut client, "C", "C1", "REL_C", "C2");

    // ── Phase 3: four cross-group edges from C into A and B, one foreign endpoint each ──────
    cross_group_edge(
        &mut client,
        "C",
        "C1_TO_A1",
        json!({"uuid": c1}),
        json!({"source_group_id": "A", "endpoint_name": "A1"}),
    );
    cross_group_edge(
        &mut client,
        "C",
        "C1_TO_B1",
        json!({"uuid": c1}),
        json!({"source_group_id": "B", "endpoint_name": "B1"}),
    );
    cross_group_edge(
        &mut client,
        "C",
        "C2_TO_A2",
        json!({"uuid": c2}),
        json!({"source_group_id": "A", "endpoint_name": "A2"}),
    );
    cross_group_edge(
        &mut client,
        "C",
        "C2_TO_B2",
        json!({"uuid": c2}),
        json!({"source_group_id": "B", "endpoint_name": "B2"}),
    );

    // ── Phase 4: an edge owned by C with BOTH endpoints foreign — the pure layer case ───────
    cross_group_edge(
        &mut client,
        "C",
        "A1_TO_B1_VIA_C",
        json!({"source_group_id": "A", "endpoint_name": "A1"}),
        json!({"source_group_id": "B", "endpoint_name": "B1"}),
    );

    // ── Edge case (FR-003): each group's own WAL stream physically exists on disk right where
    //    routing puts it (#385) ──────────────────────────────────────────────────────────────
    // C's four cross-group edges (phase 3) and its A→B layer edge (phase 4) reference A's and
    // B's entities as endpoints, but every one of those mutations must still be routed entirely
    // to C's own stream — never A's or B's. This is checked structurally (each group's own
    // directory exists and holds its own writes) rather than by content-inspecting individual
    // WAL lines' `params`: a mutation's own `group_id` param is always the value that both
    // selects its owning writer *and* gets embedded in the CREATE statement, so re-deriving it
    // from parsed `params` would only re-confirm that fact, not test attribution independently.
    // The properties that actually can be independently verified — that a foreign group's
    // stream is untouched (the B-unchanged check below), and that pointer `binding_state`/
    // `source_group_id` (reported over the MCP API, not embedded as a bare WAL param) reflect
    // the correct group — are what assertions (e)/(j) and the B/`liminis`-stream checks cover.
    let snapshot_after_writes = wal_snapshot(&wal_dir);
    for group_id in ["A", "B", "C"] {
        assert!(
            snapshot_after_writes
                .get(group_id)
                .is_some_and(|dir| !dir.jsonl_files.is_empty()),
            "group {group_id}'s own WAL stream must exist on disk and be non-empty after \
             phase 1-4 writes (#385): dirs={:?}",
            snapshot_after_writes.keys().collect::<Vec<_>>()
        );
    }

    // ── Assertion (a): applied_seq advances per group after writes (#383) ───────────────────
    let groups_after_writes = wal_groups(&mut client);
    let group_positions = groups_after_writes
        .as_object()
        .expect("wal_groups must be a JSON object");
    for (group_id, position) in group_positions {
        assert!(
            !position["applied_seq"].is_null(),
            "applied_seq must advance per group after writes (#383): group {group_id:?} has a \
             null applied_seq: {position}"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════════════
    // New phase (issue #400 FR-002): a cross-group merge in A never rewrites C's or B's edges,
    // and C's pointer follows `merged_into` forwarding to the merge survivor (#368/#371).
    // Placed here — before the existing purge/restore/rebind sequence (phases 6-9) — so that C's
    // pointer into A is still `bound` when the merge runs, per the spec's Edge Cases ordering
    // constraint. Uses dedicated entities A3/A4 and a dedicated edge C3_TO_A3, fully decoupled
    // from A1/A2 and the four existing cross-group edges every downstream assertion in this file
    // already depends on.
    // ══════════════════════════════════════════════════════════════════════════════════════
    let a3 = assert_entity(&mut client, "A", "A3");
    let a4 = assert_entity(&mut client, "A", "A4");
    let c3 = assert_entity(&mut client, "C", "C3");
    cross_group_edge(
        &mut client,
        "C",
        "C3_TO_A3",
        json!({"uuid": c3}),
        json!({"source_group_id": "A", "endpoint_name": "A3"}),
    );

    let bindings_before_merge = c_bindings(&mut client);
    let ptr_before_merge = dst_pointer(&bindings_before_merge, "C3_TO_A3");
    assert_eq!(
        ptr_before_merge.binding_state, "bound",
        "C's pointer into A3 must already be bound before the merge runs — the spec's Edge \
         Cases section requires the merge phase run before the purge/restore/rebind sequence \
         precisely so this holds (#400): {ptr_before_merge:?}"
    );
    assert_eq!(
        ptr_before_merge.resolved_uuid.as_deref(),
        Some(a3.as_str()),
        "{ptr_before_merge:?}"
    );
    assert_eq!(
        ptr_before_merge.endpoint_name, "a3",
        "C3_TO_A3's pointer must target endpoint name \"a3\" (normalized) — sanity-checking \
         which entity this pointer actually names before its merge/rebind state is inspected: \
         {ptr_before_merge:?}"
    );

    let snapshot_before_merge = wal_snapshot(&wal_dir);

    // ── Merge A3 (alias, loses) into A4 (canonical, survives) within group A ────────────────
    let merge = client.call_tool(
        "knowledge_merge_entities",
        json!({"canonical_uuid": a4, "alias_uuids": [a3], "group_id": "A"}),
    );
    let merge_result = structured(&merge, "merge_entities(A4 <- A3)");
    assert_eq!(
        merge_result["success"],
        json!(true),
        "merge_entities(A4 <- A3) must succeed (#368/#371): {merge_result}"
    );

    // ── Assertion (k): no edge owned by C, and no edge owned by B, is rewritten or deleted by a
    //    merge performed in A — the merge's mutations land only in A's own on-disk WAL stream,
    //    the governing rule ADR-0371 states for the whole multi-stream model (#371) ───────────
    let snapshot_after_merge = wal_snapshot(&wal_dir);
    for group_id in ["B", "C"] {
        assert_eq!(
            snapshot_before_merge.get(group_id).map(|d| &d.jsonl_files),
            snapshot_after_merge.get(group_id).map(|d| &d.jsonl_files),
            "a merge in A must not rewrite group {group_id}'s WAL stream (#371): before={:?} \
             after={:?}",
            snapshot_before_merge.get(group_id),
            snapshot_after_merge.get(group_id)
        );
    }
    assert!(
        merge_result["foreign_edges_skipped"].as_u64().unwrap_or(0) >= 1,
        "a merge in A must report skipping (not rewriting) the foreign edge C owns into the \
         losing alias A3 (#371): {merge_result}"
    );

    // ── Assertion (l): C's pointer follows merged_into forwarding to the surviving canonical
    //    A4 once re-examined, rather than being left dangling on the tombstoned A3 (#368) ─────
    let rebind_after_merge =
        client.call_tool("knowledge_rebind_pointers", json!({"source_group_id": "A"}));
    structured(&rebind_after_merge, "rebind_pointers(A) after merge");
    let bindings_after_merge = c_bindings(&mut client);
    let ptr_after_merge = dst_pointer(&bindings_after_merge, "C3_TO_A3");
    assert_eq!(
        ptr_after_merge.binding_state, "bound",
        "C's pointer into A must end bound to the merge survivor, not left dangling on the \
         tombstoned alias (#368/#371): {ptr_after_merge:?}"
    );
    assert_eq!(
        ptr_after_merge.resolved_uuid.as_deref(),
        Some(a4.as_str()),
        "C's pointer must follow the merged_into forwarding chain recorded on the tombstoned \
         A3 to the surviving canonical A4 (#368): {ptr_after_merge:?}"
    );

    // ── Phase 5: per-group checkpoints (#365 + #378) ─────────────────────────────────────────
    let create_a = client.call_tool(
        "knowledge_wal_mark_create",
        json!({"name": "pre_purge_A", "group_id": "A"}),
    );
    let create_a_result = structured(&create_a, "wal_mark_create A/pre_purge_A");
    let cp_seq = create_a_result["seq"].as_u64().unwrap_or_else(|| {
        panic!("wal_mark_create A/pre_purge_A returned a null seq: {create_a_result}")
    });

    let create_c = client.call_tool(
        "knowledge_wal_mark_create",
        json!({"name": "pre_purge_C", "group_id": "C"}),
    );
    structured(&create_c, "wal_mark_create C/pre_purge_C");

    let list_a = client.call_tool("knowledge_wal_mark_list", json!({"group_id": "A"}));
    let list_a_result = structured(&list_a, "wal_mark_list A");
    let checkpoint_names = |result: &Value| -> std::collections::HashSet<String> {
        result["checkpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect()
    };
    let names_a = checkpoint_names(list_a_result);

    let list_c = client.call_tool("knowledge_wal_mark_list", json!({"group_id": "C"}));
    let list_c_result = structured(&list_c, "wal_mark_list C");
    let names_c = checkpoint_names(list_c_result);

    // ── Assertion (b): per-group checkpoint lists do not aggregate across groups (#365/#378) ─
    assert!(
        !names_a.contains("pre_purge_C") && !names_c.contains("pre_purge_A"),
        "checkpoint lists must not aggregate across groups (#365/#378): A={names_a:?} C={names_c:?}"
    );

    // Snapshot B's and C's on-disk WAL directories now, before A's dry-run/purge/rebuild
    // sequence (phases 6-8) — none of those phases touch B or C, so their `.jsonl` streams must
    // come out byte-identical at the far end (FR-003's edge case: "groups not touched by a given
    // operation ... must be asserted unchanged", checked on disk, not only via `wal_groups`
    // positions).
    let snapshot_before_purge = wal_snapshot(&wal_dir);

    // ── Phase 6: delete_by_group(A) dry-run (#361) ───────────────────────────────────────────
    let dry_run = client.call_tool(
        "knowledge_delete_by_group",
        json!({"group_ids": ["A"], "dry_run": true}),
    );
    let dry_run_result = structured(&dry_run, "delete_by_group(A) dry-run");
    let impacted_groups: Vec<&str> = dry_run_result["unbound_impacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["group_id"].as_str())
        .collect();

    // ── Assertion (c): dry-run names the owning (layer) group in unbound_impacts (#361) ─────
    assert!(
        impacted_groups.contains(&"C"),
        "the dry-run preview of purging A must report an unbound impact naming the owning \
         layer group C (#361): unbound_impacts={:?}",
        dry_run_result["unbound_impacts"]
    );

    // ── Phase 7: purge group A for real (#361) ───────────────────────────────────────────────
    let purge = client.call_tool(
        "knowledge_delete_by_group",
        json!({"group_ids": ["A"], "confirm": true}),
    );
    structured(&purge, "delete_by_group(A) purge");

    let after_purge = c_bindings(&mut client);

    // ── Assertion (d): C's cross-group edges survive A's purge, are not deleted (#361/#385) ──
    for edge_name in ["C1_TO_A1", "C2_TO_A2", "A1_TO_B1_VIA_C"] {
        assert!(
            after_purge.contains_key(edge_name),
            "C's cross-group edge {edge_name:?} (which points into purged group A) must \
             survive A's purge, not be deleted (#361/#385): after_purge={after_purge:?}"
        );
    }

    // ── Assertion (e): C's pointers into A go unbound after A's purge, while C's pointers into
    //    B (never touched) stay bound (#369) ─────────────────────────────────────────────────
    let into_a_after_purge = binding_states_into(&after_purge, "A");
    assert!(
        !into_a_after_purge.is_empty() && into_a_after_purge.iter().all(|s| *s == "unbound"),
        "C's pointers into A must all be reported unbound after A's purge (#369): \
         into_a={into_a_after_purge:?} after_purge={after_purge:?}"
    );
    let into_b_after_purge = binding_states_into(&after_purge, "B");
    assert!(
        !into_b_after_purge.is_empty() && into_b_after_purge.iter().all(|s| *s == "bound"),
        "C's pointers into B must remain bound after A's purge, since B was never touched by \
         it (#369/#385): into_b={into_b_after_purge:?} after_purge={after_purge:?}"
    );

    // ── Assertion (f): purging A's graph does not delete A's own WAL stream (#385) ──────────
    let snapshot_after_purge = wal_snapshot(&wal_dir);
    let a_stream_survives = snapshot_after_purge
        .get("A")
        .map(|dir| !dir.jsonl_files.is_empty())
        .unwrap_or(false);
    assert!(
        a_stream_survives,
        "purging A's graph content must not delete A's own WAL directory/stream — a graph \
         purge and a WAL-stream deletion are different operations (#385): WAL dirs on disk = \
         {:?}",
        snapshot_after_purge.keys().collect::<Vec<_>>()
    );

    // ── Phase 8: rebuild group A from its own WAL, replayed up to its pre-purge checkpoint ──
    let positions_before_rebuild = wal_groups(&mut client);

    // A progress token makes `knowledge_rebuild_from_wal` run synchronously within this one
    // request (see `handle_rebuild_from_wal`'s `is_streaming` branch in
    // `crates/core/src/handlers.rs`) instead of returning a `job_id` to poll — the same pattern
    // `mcp_real_corpus_admin_data_e2e.rs` uses for the identical call, with the same generous
    // 600s timeout. This test's tiny (2-entity) dataset still completes in low single-digit
    // seconds either way, but this removes the fixed short poll budget as a source of CI
    // flakiness on a loaded/slow runner.
    let rebuild_resp = client.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({"group_id": "A", "from_seq": 0, "to_seq": cp_seq, "force_clear": true}),
        "multistream-rebuild-A",
        Duration::from_secs(600),
    );
    let rebuild_result = structured(&rebuild_resp, "rebuild_from_wal(A)");
    assert_eq!(
        rebuild_result["success"],
        json!(true),
        "rebuild_from_wal(A) must complete successfully (#378): {rebuild_result}"
    );

    let positions_after_rebuild = wal_groups(&mut client);

    // ── Assertions (g)/(h): replaying A's own WAL leaves B's and C's WAL positions untouched
    //    (#378) ────────────────────────────────────────────────────────────────────────────
    for group_id in ["B", "C"] {
        assert_eq!(
            positions_before_rebuild.get(group_id),
            positions_after_rebuild.get(group_id),
            "replaying group A's own WAL must leave group {group_id}'s WAL position \
             byte-identical (#378): before={:?} after={:?}",
            positions_before_rebuild.get(group_id),
            positions_after_rebuild.get(group_id)
        );
    }

    // ── Edge case (FR-003): B's on-disk WAL directory is untouched by A's
    //    dry-run/purge/rebuild (#385) ─────────────────────────────────────────────────────────
    // Only B qualifies as "not party to the operation" here (matching the spec's own edge-case
    // example): C holds cross-group pointers *into* A, so purging A legitimately mutates C's own
    // RelatesToNode_ records (flipping binding_state to unbound) — a correct write to C's own
    // stream, not #385-style misattribution. B has no pointers into A, so its stream must be
    // byte-identical before and after.
    let snapshot_after_rebuild = wal_snapshot(&wal_dir);
    assert_eq!(
        snapshot_before_purge.get("B").map(|d| &d.jsonl_files),
        snapshot_after_rebuild.get("B").map(|d| &d.jsonl_files),
        "group B's on-disk WAL stream must be byte-identical before and after A's \
         dry-run/purge/rebuild sequence, since B was never party to any of those operations \
         (#385): the bug this guards against is a mutation belonging to one group landing in \
         another group's stream on disk, which is invisible from knowledge_status alone"
    );

    // ── Assertion (i): A's entities are restored by replaying A's own WAL (#378) ────────────
    let nodes_a = client.call_tool("knowledge_get_nodes_by_group", json!({"group_ids": ["A"]}));
    let nodes_a_result = structured(&nodes_a, "get_nodes_by_group(A)");
    let node_count = nodes_a_result["nodes"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    assert!(
        node_count >= 2,
        "group A's entities (A1, A2) must be restored by replaying A's own WAL (#378): \
         expected at least 2 nodes, got {node_count}: {nodes_a_result}"
    );

    // ── Phase 9: rebind C's pointers into A (#369 / #378) ────────────────────────────────────
    let rebind = client.call_tool("knowledge_rebind_pointers", json!({"source_group_id": "A"}));
    structured(&rebind, "rebind_pointers(A)");

    let after_rebind = c_bindings(&mut client);

    // ── Assertion (j): C's pointers into A re-bind after A is rehydrated (#369, #392) ───────
    // `group_purge::purge_groups`'s forced rebind stamps `bound_at_seq` at A's post-purge (high)
    // applied_seq and flips the pointers to `unbound`; `knowledge_rebuild_from_wal`'s
    // checkpoint-bounded restore then rewinds A's applied_seq back down to the pre-purge
    // checkpoint (a lower number). Before #392 was fixed, the non-forced
    // `knowledge_rebind_pointers` call above saw a stamped `bound_at_seq` that was still >= the
    // (now-lower) current applied_seq and skipped re-checking, leaving the pointer `unbound`
    // even though A's data was back. The gate now also re-checks any pointer whose current
    // `binding_state` is `unbound`/`ambiguous` regardless of `bound_at_seq`, so this composition
    // repairs correctly through the public API alone.
    let into_a_after_rebind = binding_states_into(&after_rebind, "A");
    assert!(
        !into_a_after_rebind.is_empty() && into_a_after_rebind.iter().all(|s| *s == "bound"),
        "expected C's pointers into A to be bound after knowledge_rebind_pointers, following \
         A's purge and checkpoint-bounded restore (#392) — into_a={into_a_after_rebind:?} \
         after_rebind={after_rebind:?}"
    );

    // ── Edge case (FR-003 / Background #385): no mutation in this test ever targets the
    //    default group, so no `liminis` WAL stream should exist on disk at all ────────────────
    // This is #385's exact original failure mode: `delete_by_group` and `rebind_pointers`
    // writing other groups' mutations into the default stream instead of the owning group's own
    // — invisible from `knowledge_status` (which reports per-group positions, not directory
    // existence) and invisible from every assertion above, which only inspect A/B/C's own
    // streams and never check that a stray fourth stream didn't appear alongside them.
    let final_snapshot = wal_snapshot(&wal_dir);
    assert!(
        !final_snapshot.contains_key(lcg_core::DEFAULT_GROUP_ID),
        "no operation in this test targets the default group {:?}, so no WAL stream for it \
         should exist on disk (#385): a stray directory there would mean a mutation belonging \
         to A/B/C was misattributed to the default group instead. dirs={:?}",
        lcg_core::DEFAULT_GROUP_ID,
        final_snapshot.keys().collect::<Vec<_>>()
    );

    // ══════════════════════════════════════════════════════════════════════════════════════
    // New phase (issue #400 FR-003): ambiguous endpoint resolution (#369). No pointer built
    // through the assert API can ever be ambiguous, since knowledge_assert_entity is idempotent
    // by (name, group_id) (#379 FR-011) — it can never create a second active entity sharing an
    // existing name. Two same-named active entities are constructed instead by raw-injecting WAL
    // lines directly into A's on-disk stream and importing them via an incremental (non-reset)
    // knowledge_rebuild_from_wal call, mirroring how crates/core's own
    // `resolve_endpoint_ambiguous_when_two_active_matches` test bypasses the assert API too.
    // Placed here — after the existing purge/restore/rebind sequence (phases 6-9) rather than
    // before it — because phase 7's purge is itself a live write into A's own WAL stream
    // (`knowledge_delete_by_group` flushes each purged group's own deletions to its own writer,
    // issue #385/ADR-0385). Raw-injecting a file into A's directory and then letting that live
    // purge-write land after it, ahead of phase 8's checkpoint-bounded from-scratch replay,
    // produced an unrecoverable hang in the spawned process during development of this issue —
    // the on-disk state and the live `WalWriter`'s cached position diverge in a way this file's
    // process-boundary harness cannot safely resolve. Running this phase (and FR-001 below) only
    // after phase 9 — the last point at which no further live write to A ever occurs — avoids
    // that interaction entirely, matching the safety invariant `crates/core/tests/
    // wal_generation_reset.rs` already relies on for the same kind of raw manipulation one layer
    // down (no live write to a group after it has been externally manipulated on disk).
    // ══════════════════════════════════════════════════════════════════════════════════════
    let positions_before_dup = wal_groups(&mut client);
    let a_max_seq_before_dup = positions_before_dup["A"]["max_seq"]
        .as_u64()
        .unwrap_or_else(|| {
            panic!(
                "group A must have a known max_seq before the ambiguous-pair injection: \
             {positions_before_dup}"
            )
        });

    wal_manipulate::write_group_wal(
        &wal_dir,
        "A",
        "9000-ambiguous.jsonl",
        &[
            wal_manipulate::entity_wal_line(a_max_seq_before_dup + 1, "a-dup-1", "A_DUP", "A"),
            wal_manipulate::entity_wal_line(a_max_seq_before_dup + 2, "a-dup-2", "A_DUP", "A"),
        ],
    );

    let import_dup = client.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({"group_id": "A", "from_seq": a_max_seq_before_dup + 1}),
        "multistream-import-ambiguous-A",
        Duration::from_secs(60),
    );
    let import_dup_result = structured(
        &import_dup,
        "rebuild_from_wal(A) importing raw-injected ambiguous pair",
    );
    assert_eq!(
        import_dup_result["success"],
        json!(true),
        "the incremental import of the raw-injected ambiguous pair must succeed: \
         {import_dup_result}"
    );
    assert_ne!(
        import_dup_result["reset_detected"],
        json!(true),
        "appending a new file to A's existing generation must never itself be detected as a \
         reset (#387): {import_dup_result}"
    );

    let c4 = assert_entity(&mut client, "C", "C4");
    cross_group_edge(
        &mut client,
        "C",
        "C4_TO_A_DUP",
        json!({"uuid": c4}),
        json!({"source_group_id": "A", "endpoint_name": "A_DUP"}),
    );

    // ── Assertion (m): an endpoint name shared by two active entities in A resolves ambiguous,
    //    never a silently picked winner (#369) ──────────────────────────────────────────────
    let bindings_ambiguous = c_bindings(&mut client);
    let ptr_ambiguous = dst_pointer(&bindings_ambiguous, "C4_TO_A_DUP");
    assert_eq!(
        ptr_ambiguous.binding_state, "ambiguous",
        "an endpoint name (A_DUP) shared by two active entities in A must resolve ambiguous, \
         never silently bind to an arbitrary winner (#369): {ptr_ambiguous:?}"
    );
    assert!(
        ptr_ambiguous.resolved_uuid.is_none(),
        "an ambiguous pointer must carry no resolved_uuid (#369): {ptr_ambiguous:?}"
    );

    // ── Assertion (n): knowledge_rebind_pointers re-examines an ambiguous pointer rather than
    //    skipping it as already resolved (#369/#392) ────────────────────────────────────────
    let rebind_ambiguous =
        client.call_tool("knowledge_rebind_pointers", json!({"source_group_id": "A"}));
    let rebind_ambiguous_result = structured(
        &rebind_ambiguous,
        "rebind_pointers(A) re-examining the ambiguous pointer",
    );
    assert!(
        rebind_ambiguous_result["ambiguous"].as_u64().unwrap_or(0) >= 1,
        "knowledge_rebind_pointers must re-examine (re-resolve), not skip, a pointer that is \
         already reported ambiguous — only a currently-Bound pointer is eligible for the \
         staleness-skip gate (#369/#392): {rebind_ambiguous_result}"
    );
    let bindings_after_ambiguous_rebind = c_bindings(&mut client);
    let ptr_after_ambiguous_rebind = dst_pointer(&bindings_after_ambiguous_rebind, "C4_TO_A_DUP");
    assert_eq!(
        ptr_after_ambiguous_rebind.binding_state, "ambiguous",
        "the pointer must still be reported ambiguous after rebind_pointers re-examines it, not \
         silently coerced to bound (#369): {ptr_after_ambiguous_rebind:?}"
    );

    // ══════════════════════════════════════════════════════════════════════════════════════
    // New phase (issue #400 FR-001): upstream stream generation reset self-heals a layer
    // graph's pointers (#387). Republishes A's entire stream under a new `.wal-generation.json`
    // identity with fresh entity identities under the same names (A1, A2) — not merely appending
    // more lines to the existing generation. The reset self-heal path (FR-010) must re-bind
    // every one of C's pointers into A by name, not by the now-stale pre-reset UUIDs. Placed
    // last of the two raw-manipulation phases against A, and after every existing live write to
    // A (phase 7's purge is the last one, per the FR-003 phase's own comment above) — so nothing
    // ever writes to A again after this reset, and this phase's own wipe-everything semantics
    // cleanly subsumes the FR-003 phase's scratch "A_DUP" entities.
    // ══════════════════════════════════════════════════════════════════════════════════════
    let bindings_before_reset = c_bindings(&mut client);
    let a1_uuid_before_reset = dst_pointer(&bindings_before_reset, "C1_TO_A1")
        .resolved_uuid
        .clone()
        .expect("C1_TO_A1 must be bound to A1 before the reset");
    let a2_uuid_before_reset = dst_pointer(&bindings_before_reset, "C2_TO_A2")
        .resolved_uuid
        .clone()
        .expect("C2_TO_A2 must be bound to A2 before the reset");

    let positions_before_reset = wal_groups(&mut client);
    let snapshot_before_reset = wal_snapshot(&wal_dir);

    wal_manipulate::reset_group_stream(
        &wal_dir,
        "A",
        "0000-reset.jsonl",
        &[
            wal_manipulate::entity_wal_line(0, "a1-reset", "A1", "A"),
            wal_manipulate::entity_wal_line(1, "a2-reset", "A2", "A"),
        ],
        "post-reset-generation",
    );

    let reset_resp = client.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({"group_id": "A", "from_seq": 0}),
        "multistream-reset-A",
        Duration::from_secs(600),
    );
    let reset_result = structured(&reset_resp, "rebuild_from_wal(A) after generation reset");

    // ── Assertion (o): a republished stream under a new generation identity is detected as a
    //    reset, not replayed incrementally across it (#387) ─────────────────────────────────
    assert_eq!(
        reset_result["success"],
        json!(true),
        "the reset self-heal replay must succeed: {reset_result}"
    );
    assert_eq!(
        reset_result["reset_detected"],
        json!(true),
        "republishing A's stream under a new .wal-generation.json identity with republished \
         content must be detected as a reset, not replayed incrementally across it (#387): \
         {reset_result}"
    );
    assert_eq!(
        reset_result["generation"],
        json!("post-reset-generation"),
        "{reset_result}"
    );

    // ── Assertion (p): C's pointers into A end bound to A's newly republished entities via the
    //    reset self-heal path, resolved by name, not the stale pre-reset UUIDs (#387) ─────────
    let bindings_after_reset = c_bindings(&mut client);
    let a1_after_reset = dst_pointer(&bindings_after_reset, "C1_TO_A1");
    assert_eq!(a1_after_reset.binding_state, "bound", "{a1_after_reset:?}");
    assert_eq!(
        a1_after_reset.resolved_uuid.as_deref(),
        Some("a1-reset"),
        "{a1_after_reset:?}"
    );
    assert_ne!(
        a1_after_reset.resolved_uuid.as_deref(),
        Some(a1_uuid_before_reset.as_str()),
        "C1_TO_A1 must rebind to A's newly republished A1, resolved by name — not remain \
         pinned to the stale pre-reset uuid (#387): {a1_after_reset:?}"
    );
    let a2_after_reset = dst_pointer(&bindings_after_reset, "C2_TO_A2");
    assert_eq!(a2_after_reset.binding_state, "bound", "{a2_after_reset:?}");
    assert_eq!(
        a2_after_reset.resolved_uuid.as_deref(),
        Some("a2-reset"),
        "{a2_after_reset:?}"
    );
    assert_ne!(
        a2_after_reset.resolved_uuid.as_deref(),
        Some(a2_uuid_before_reset.as_str()),
        "C2_TO_A2 must rebind to A's newly republished A2, resolved by name — not remain \
         pinned to the stale pre-reset uuid (#387): {a2_after_reset:?}"
    );

    // ── Assertion (q): group B's WAL stream, position, and generation are all unchanged by a
    //    reset scoped to group A alone (#387) ───────────────────────────────────────────────
    let positions_after_reset = wal_groups(&mut client);
    assert_eq!(
        positions_before_reset.get("B"),
        positions_after_reset.get("B"),
        "group B's WAL position/generation must be byte-identical before and after A's \
         generation reset — the reset is scoped to group A alone (#387): before={:?} after={:?}",
        positions_before_reset.get("B"),
        positions_after_reset.get("B")
    );
    let snapshot_after_reset = wal_snapshot(&wal_dir);
    assert_eq!(
        snapshot_before_reset.get("B").map(|d| &d.jsonl_files),
        snapshot_after_reset.get("B").map(|d| &d.jsonl_files),
        "group B's on-disk WAL stream must be byte-identical before and after A's generation \
         reset (#387)"
    );

    // ══════════════════════════════════════════════════════════════════════════════════════
    // New phase (issue #400 FR-006 / #414): a WAL stream that arrives with no generation
    // sidecar at all is detected, not silently inferred. Every stream this test has built so far
    // has a generation by construction (lcg mints one on first write) — #414's defect was a
    // stream arriving with *no* generation at all, a distinct condition the FR-001 reset phase
    // above cannot reach (that phase exercises a known-to-known mismatch). Simulated by deleting
    // B's `.wal-generation.json` sidecar directly — the shape a publish step has in practice when
    // it globs `*.jsonl` only and silently drops the dot-namespaced file (#414's confirmed root
    // cause) — never by any lcg API call, since no API produces this state. Targets B: by this
    // point in the file, every comparison that depends on B staying untouched (assertions
    // (g)/(h) and this file's own new FR-001 assertion (q)) has already run.
    // ══════════════════════════════════════════════════════════════════════════════════════
    let positions_before_missing_sidecar = wal_groups(&mut client);
    let a_entry_before_missing_sidecar = positions_before_missing_sidecar.get("A").cloned();
    let c_entry_before_missing_sidecar = positions_before_missing_sidecar.get("C").cloned();
    let b_entry_before_missing_sidecar = positions_before_missing_sidecar
        .get("B")
        .cloned()
        .expect("group B must already have a wal_groups entry before this phase");
    assert!(
        !b_entry_before_missing_sidecar["generation"].is_null(),
        "group B must have a known generation before this phase removes its sidecar, or this \
         phase would not be exercising the transition it claims to (#414): \
         {b_entry_before_missing_sidecar}"
    );

    wal_manipulate::remove_group_generation(&wal_dir, "B");

    // ── Assertion (r): knowledge_status reports the missing-sidecar condition for B,
    //    distinguishable from both a group with a known generation and a group with no stream
    //    at all (#414 / SC-004) ────────────────────────────────────────────────────────────
    let positions_after_missing_sidecar = wal_groups(&mut client);
    let b_entry_after_missing_sidecar =
        positions_after_missing_sidecar.get("B").unwrap_or_else(|| {
            panic!(
                "group B's wal_groups entry must still be present (content on disk, only the \
                 generation sidecar missing) — distinct from a group with no stream at all, \
                 which has no entry at all (#414): {positions_after_missing_sidecar}"
            )
        });
    assert!(
        b_entry_after_missing_sidecar["generation"].is_null(),
        "group B's reported generation must be null once its sidecar is missing (#414): \
         {b_entry_after_missing_sidecar}"
    );
    assert_eq!(
        b_entry_after_missing_sidecar["applied_seq"], b_entry_before_missing_sidecar["applied_seq"],
        "removing only the generation sidecar must not disturb B's recorded applied_seq — the \
         group is not treated as having no stream (#414): before={b_entry_before_missing_sidecar} \
         after={b_entry_after_missing_sidecar}"
    );
    assert!(
        !positions_after_missing_sidecar
            .as_object()
            .is_some_and(|m| m.contains_key("no-such-group-400")),
        "a group with no WAL directory at all must have no wal_groups entry whatsoever — the \
         contrasting case that makes B's present-but-null entry distinguishable as 'unknown \
         generation', not merely 'no stream yet' (#414/SC-004): {positions_after_missing_sidecar}"
    );

    // ── Assertion (s): B's replay refuses outright now that #414's FR-002 guard has landed. B
    //    already has a recorded position (Assertion (r) above), so a subsequent
    //    knowledge_rebuild_from_wal call against its now-unknown generation must fail before any
    //    replay is attempted — including an incremental replay with no `force_clear` — rather
    //    than silently proceeding as ordinary forward progress (#414 FR-002/SC-003). This
    //    supersedes the pre-#414 "success: true, reset_detected != true" placeholder this phase
    //    asserted while #414's guard didn't exist yet. Still exercised via an incremental replay
    //    (not a `from_seq: 0`/`force_clear: true` rebuild) to keep parity with the rest of this
    //    phase's setup — the refusal fires before any downstream path branches, so the specific
    //    replay shape requested doesn't change the outcome ────────────────────────────────────
    let b_from_seq = b_entry_after_missing_sidecar["applied_seq"]
        .as_u64()
        .map(|seq| seq + 1)
        .unwrap_or(0);
    let replay_missing_sidecar = client.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({"group_id": "B", "from_seq": b_from_seq}),
        "multistream-missing-sidecar-B",
        Duration::from_secs(600),
    );
    assert_eq!(
        replay_missing_sidecar["result"]["isError"],
        json!(true),
        "rebuild_from_wal(B) with a recorded position and a missing generation sidecar must \
         refuse outright, not silently proceed as an ordinary replay (#414 FR-002): \
         {replay_missing_sidecar}"
    );
    let replay_missing_sidecar_message = replay_missing_sidecar["result"]["structuredContent"]
        ["message"]
        .as_str()
        .unwrap_or("");
    assert!(
        replay_missing_sidecar_message.contains('B')
            && replay_missing_sidecar_message.contains(".wal-generation.json"),
        "the refusal error must name the affected group and the missing sidecar (#414 FR-002): \
         {replay_missing_sidecar}"
    );

    // ── Assertion (t): groups A and C, sharing the same WAL root as B, are unaffected by B's
    //    missing-sidecar condition (#414) ────────────────────────────────────────────────────
    let positions_final = wal_groups(&mut client);
    assert_eq!(
        positions_final.get("A"),
        a_entry_before_missing_sidecar.as_ref(),
        "group A's wal_groups entry must be unaffected by B's missing generation sidecar \
         (#414): before={a_entry_before_missing_sidecar:?} after={:?}",
        positions_final.get("A")
    );
    assert_eq!(
        positions_final.get("C"),
        c_entry_before_missing_sidecar.as_ref(),
        "group C's wal_groups entry must be unaffected by B's missing generation sidecar \
         (#414): before={c_entry_before_missing_sidecar:?} after={:?}",
        positions_final.get("C")
    );

    client.shutdown();
}
