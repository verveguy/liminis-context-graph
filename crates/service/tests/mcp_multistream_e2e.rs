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
//! Phase 9's assertion is a deliberate exception: #392 (a staleness gate in
//! `crates/core/src/cross_group.rs::rebind_pointers_impl`) is not yet fixed, so per FR-006 that
//! assertion checks today's actual (broken) behavior with a `TODO(#392)` marker, rather than
//! being `#[ignore]`d or silently dropped — see that assertion's comment for the mechanism.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

mod common;
use common::wal_inspect::wal_snapshot;
use common::{binary_path, spawn_stub_embedder, McpClient};

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

/// Reads group `C`'s `RelatesToNode_` rows and returns, per edge name, the `binding_state` of
/// each cross-group pointer side (`"src"`/`"dst"`) it carries — the same shape the reference
/// Python harness's `c_bindings()` produces. `handle_query_cypher` returns `rows` as
/// `Vec<Vec<String>>` (stringified columns, not maps — confirmed against
/// `crates/core/src/handlers.rs`), so each row is indexed positionally: `[0]` is `rn.name`,
/// `[1]` is `rn.attributes` (a JSON string).
fn c_bindings(client: &mut McpClient) -> HashMap<String, HashMap<String, String>> {
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
                let state = ptr["binding_state"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                sides.insert(side.clone(), state);
            }
        }
        out.insert(name, sides);
    }
    out
}

fn any_unbound(bindings: &HashMap<String, HashMap<String, String>>) -> bool {
    bindings
        .values()
        .any(|sides| sides.values().any(|s| s == "unbound"))
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

    // ── Assertion (e): C's pointers into A go unbound after A's purge (#369) ────────────────
    assert!(
        any_unbound(&after_purge),
        "C's pointers into A must be reported unbound after A's purge (#369): \
         after_purge={after_purge:?}"
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

    let rebuild = client.call_tool(
        "knowledge_rebuild_from_wal",
        json!({"group_id": "A", "from_seq": 0, "to_seq": cp_seq, "force_clear": true}),
    );
    let rebuild_result = structured(&rebuild, "rebuild_from_wal(A)");
    let job_id = rebuild_result["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("rebuild_from_wal(A) returned no job_id: {rebuild_result}"))
        .to_string();

    let mut final_job_status = None;
    for _ in 0..40 {
        let poll = client.call_tool("knowledge_rebuild_status", json!({"job_id": job_id}));
        let poll_result = structured(&poll, "rebuild_status(A)");
        let status = poll_result["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status == "completed" || status == "failed" {
            final_job_status = Some((status, poll_result.clone()));
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let (final_status, final_result) = final_job_status.unwrap_or_else(|| {
        panic!(
            "rebuild_from_wal(A) job {job_id} did not reach a terminal status within the \
             polling budget (SC-001)"
        )
    });
    assert_eq!(
        final_status, "completed",
        "rebuild_from_wal(A) must complete successfully (#378): {final_result}"
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

    // ── Assertion (j): C's pointers into A re-bind after A is rehydrated (#369) ─────────────
    // TODO(#392): `rebind_pointers_impl` (crates/core/src/cross_group.rs) skips re-checking a
    // pointer whenever its stamped `bound_at_seq` is >= the source group's *current* applied_seq
    // (its staleness gate). `group_purge::purge_groups`'s forced rebind stamps `bound_at_seq` at
    // A's post-purge (high) applied_seq; `knowledge_rebuild_from_wal`'s checkpoint-bounded
    // restore then rewinds A's applied_seq back down to the pre-purge checkpoint (a lower
    // number). The non-forced `knowledge_rebind_pointers` call above sees a stamped
    // `bound_at_seq` that is still >= the (now-lower) current applied_seq and skips
    // re-checking — so the pointer never leaves `unbound`, even though A's data is back. Per
    // FR-006, this assertion intentionally checks today's actual (broken) behavior rather than
    // being `#[ignore]`d, so the fix for #392 is a deliberate, visible diff to this test: once
    // fixed, `any_unbound(&after_rebind)` will be `false` and this assertion must be flipped to
    // require that (removing this TODO), not deleted.
    assert!(
        any_unbound(&after_rebind),
        "TODO(#392): expected C's pointers into A to still be reported unbound after \
         knowledge_rebind_pointers, due to the known rebind_pointers staleness-gate bug (#392) \
         — if this assertion now fails, #392 may have been fixed upstream; flip this assertion \
         to require every pointer be 'bound' and remove the TODO(#392) comment above. \
         after_rebind={after_rebind:?}"
    );

    client.shutdown();
}
