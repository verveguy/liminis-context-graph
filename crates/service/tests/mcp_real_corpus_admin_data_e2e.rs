//! MCP admin/lifecycle e2e suite over the real-corpus WAL fixture (issue #236) — data-plane
//! half: `knowledge_dump_wal`, `knowledge_rebuild_from_wal` (+ progress notifications),
//! `knowledge_prepare_checkpoint`, and `knowledge_build_indices`. The process-lifecycle half
//! (degraded-mode recovery, `knowledge_close`/attached-mode shutdown, admin scope gating) lives
//! in `mcp_real_corpus_admin_lifecycle_e2e.rs`.
//!
//! Completes the MCP e2e trilogy (#217 → #234 → #235 → #236): the admin/lifecycle surface is
//! the engine's data-safety machinery — the tools an operator reaches for specifically *after*
//! something has gone wrong, or as the safety net taken *before* a risky operation — and until
//! this issue it was exercised only against small synthetic graphs, never through MCP
//! (`tools/call`) and never at a scale anywhere near where these operations are expensive
//! enough for bugs to hide.
//!
//! **Isolation (Edge Cases)**: the fixture is seeded once (`seed_real_corpus_workspace`,
//! ~60-140s), then each user-story block below gets its own independent copy via
//! `SeededWorkspace::fork()` — a cheap recursive filesystem copy, not a from-scratch reseed —
//! so one block's mutation (a dump, a rebuild, an index drop) can never leak into another's
//! assertions.
//!
//! **Zero LLM/embedder calls**: as in `mcp_real_corpus_e2e.rs`/`mcp_real_corpus_mutation_e2e.rs`,
//! this suite never sets `ANTHROPIC_API_KEY` and every `--embedder-http` URL below points only
//! at this suite's own loopback stub embedder — WAL replay/dump/rebuild is 100% mechanical
//! Cypher re-execution over already-embedded content (FR-015).
//!
//! **Runtime budget (FR-016)**: this file performs two full WAL-replay cycles (User Story 1's
//! dump→rebuild-into-a-new-database, and User Story 5's checkpoint→rebuild-verify) plus one
//! full rebuild driven with a progress token (User Story 2 / User Story 4 Scenario 1) — each
//! ~60-140s per `real_corpus_wal/README.md` — so this test is expected to run several minutes.
//! `#[ignore]`d and excluded from the default `cargo test --release` gate; wired into CI on
//! push-to-main / `workflow_dispatch` (`.github/workflows/real-corpus-e2e.yml`).
#![cfg(unix)]

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

mod common;
use common::real_corpus::seed_real_corpus_workspace;
use common::{binary_path, spawn_stub_embedder, McpClient};

/// Every block needs both the seven admin tools and the read-scope golden-query tools
/// (`knowledge_status`, `knowledge_find_entities`, `knowledge_list_entities`,
/// `knowledge_get_episodes`, `knowledge_get_edges_by_uuids`).
const ADMIN_READ_SCOPE: &str = "--scope=admin,read";
/// User Story 6 additionally needs `knowledge_query_cypher` to induce a genuine, reversible
/// index-build failure (see that block's own comment for why).
const ADMIN_READ_CYPHER_SCOPE: &str = "--scope=admin,read,cypher";

/// Extracts `structuredContent` from a `tools/call` response, asserting it wasn't a tool-level
/// error first — mirrors `mcp_real_corpus_e2e.rs`'s helper of the same name.
fn structured<'a>(resp: &'a Value, context: &str) -> &'a Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    &resp["result"]["structuredContent"]
}

fn as_uuid_set(v: &Value, key: &str) -> HashSet<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("expected {key} to be an array: {v}"))
        .iter()
        .map(|n| {
            n["uuid"]
                .as_str()
                .unwrap_or_else(|| panic!("expected {key} element to have a string `uuid`: {n}"))
                .to_string()
        })
        .collect()
}

/// Spawns a `--mcp-stdio` process against a caller-chosen `db_path`/`wal_dir` pair, independent
/// of any `SeededWorkspace` — needed only by User Story 1, which rebuilds a dump's output into a
/// brand-new database distinct from any already-seeded workspace's own files.
fn spawn_fresh(
    db_path: &Path,
    wal_dir: &Path,
    embedder_url: &str,
    extra_args: &[&str],
) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .args(["--mcp-stdio", "--embedder-http", embedder_url])
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
        .args(extra_args);
    McpClient::spawn(cmd)
}

/// Asserts `arr`'s `uuid` fields form a set of exactly `expected_count` distinct values —
/// mirrors `mcp_real_corpus_e2e.rs`'s enumeration-correctness technique (#217/#234).
fn assert_exact_uuid_enumeration(v: &Value, key: &str, expected_count: i64, context: &str) {
    let arr = v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{context}: expected {key} to be an array: {v}"));
    assert_eq!(
        arr.len() as i64,
        expected_count,
        "{context}: expected exactly {expected_count} {key}, got {}",
        arr.len()
    );
    let uuids = as_uuid_set(v, key);
    assert_eq!(
        uuids.len(),
        arr.len(),
        "{context}: found duplicate UUIDs among {key}"
    );
}

// #[ignore]: seeds the fixture once (~60-140s), then runs every admin data-plane story against
// independently forked copies. Run explicitly via `cargo test -p lcg-service --release --test
// mcp_real_corpus_admin_data_e2e -- --ignored`; wired into CI on push-to-main / workflow_dispatch.
#[test]
#[ignore]
fn mcp_admin_data_operations_over_real_corpus_fixture() {
    let port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let base = seed_real_corpus_workspace(&embedder_url);
    let expected = base.expected.clone();
    let group_id = base.group_id().to_string();
    let entity_total = expected["entity_count"].as_i64().unwrap();
    let episode_total = expected["episode_count"].as_i64().unwrap();

    // ── Sample content drawn from the unmutated base, before any fork exists ─────────────
    // Reused by User Story 1's dump→rebuild lossless-round-trip content assertions.
    let (sample_entities, sample_episode_uuids) = {
        let mut reader = base.spawn_reader(&embedder_url, &["--scope=read"]);
        reader.initialize();

        let entities_resp = reader.call_tool(
            "knowledge_list_entities",
            json!({"group_ids": [group_id.clone()], "num_results": entity_total + 100}),
        );
        let entities = structured(&entities_resp, "list_entities (sampling)")["nodes"]
            .as_array()
            .unwrap()
            .clone();
        let sample_entities: Vec<(String, String)> = entities
            .iter()
            .take(5)
            .map(|n| {
                (
                    n["uuid"].as_str().unwrap().to_string(),
                    n["name"].as_str().unwrap().to_string(),
                )
            })
            .collect();

        let episodes_resp = reader.call_tool(
            "knowledge_get_episodes",
            json!({"group_id": group_id.clone(), "last_n": episode_total + 50}),
        );
        let episodes = structured(&episodes_resp, "get_episodes (sampling)")["episodes"]
            .as_array()
            .unwrap()
            .clone();
        let sample_episode_uuids: Vec<String> = episodes
            .iter()
            .take(3)
            .map(|e| e["uuid"].as_str().unwrap().to_string())
            .collect();

        reader.shutdown();
        (sample_entities, sample_episode_uuids)
    };
    let sample_edges: Vec<Value> = expected["relation_type_samples"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !sample_edges.is_empty(),
        "fixture must record relation_type_samples"
    );

    // ── User Story 1 (FR-002, FR-003): dump → rebuild-into-a-new-database round trip ─────
    {
        let fork = base.fork();
        let mut admin = fork.spawn_reader(&embedder_url, &[ADMIN_READ_SCOPE]);
        admin.initialize();

        let dump_dir = fork.dir.path().join("dump-full");
        let dump_resp = admin.call_tool(
            "knowledge_dump_wal",
            json!({"target_dir": dump_dir.to_str().unwrap()}),
        );
        let dump_result = structured(&dump_resp, "knowledge_dump_wal (full)");
        assert_eq!(dump_result["success"], json!(true), "{dump_result}");
        let full_nodes_dumped = dump_result["nodes_dumped"].as_i64().unwrap();
        let full_edges_dumped = dump_result["edges_dumped"].as_i64().unwrap();
        assert!(
            full_nodes_dumped > 0 && full_edges_dumped > 0,
            "expected a nonzero full-fixture dump: {dump_result}"
        );
        assert!(
            dump_result["files_written"].as_i64().unwrap() > 0,
            "expected the dump to produce at least one compacted WAL file: {dump_result}"
        );

        // FR-003: a dump scoped to the fixture's one real group must produce identical dumped
        // counts to the unscoped dump above (proving correct inclusion), and a dump scoped to a
        // group absent from the fixture must dump nothing at all (proving the filter isn't a
        // silent pass-through). Neither needs its own rebuild cycle — the round-trip mechanics
        // are already proven by the unscoped dump below; these two only need to prove the
        // *filter* behaves correctly at dump time.
        let dump_group_dir = fork.dir.path().join("dump-group");
        let dump_group_resp = admin.call_tool(
            "knowledge_dump_wal",
            json!({"group_id": group_id.clone(), "target_dir": dump_group_dir.to_str().unwrap()}),
        );
        let dump_group_result = structured(&dump_group_resp, "knowledge_dump_wal (real group_id)");
        assert_eq!(
            dump_group_result["nodes_dumped"], dump_result["nodes_dumped"],
            "group-scoped dump of the fixture's only group must match the unscoped dump: \
             {dump_group_result} vs {dump_result}"
        );
        assert_eq!(
            dump_group_result["edges_dumped"], dump_result["edges_dumped"],
            "group-scoped dump of the fixture's only group must match the unscoped dump: \
             {dump_group_result} vs {dump_result}"
        );

        let dump_nonexistent_dir = fork.dir.path().join("dump-nonexistent");
        let dump_nonexistent_resp = admin.call_tool(
            "knowledge_dump_wal",
            json!({
                "group_id": "nonexistent_group_for_236",
                "target_dir": dump_nonexistent_dir.to_str().unwrap()
            }),
        );
        let dump_nonexistent_result = structured(
            &dump_nonexistent_resp,
            "knowledge_dump_wal (nonexistent group_id)",
        );
        assert_eq!(
            dump_nonexistent_result["nodes_dumped"],
            json!(0),
            "dump scoped to a nonexistent group_id must dump zero nodes: {dump_nonexistent_result}"
        );
        assert_eq!(
            dump_nonexistent_result["edges_dumped"],
            json!(0),
            "dump scoped to a nonexistent group_id must dump zero edges: {dump_nonexistent_result}"
        );

        admin.shutdown();

        // Rebuild the unscoped dump's output into a brand-new, empty database — a separate
        // workspace from `fork`'s own db_path/wal_dir entirely.
        let rebuilt_db_path = fork.dir.path().join("rebuilt-from-dump.db");
        let mut rebuilt = spawn_fresh(
            &rebuilt_db_path,
            &dump_dir,
            &embedder_url,
            &[ADMIN_READ_SCOPE],
        );
        rebuilt.initialize();

        let rebuild_resp = rebuilt.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({}),
            "us1-dump-rebuild",
            Duration::from_secs(600),
        );
        let rebuild_result = structured(&rebuild_resp, "knowledge_rebuild_from_wal (from dump)");
        assert_eq!(rebuild_result["success"], json!(true), "{rebuild_result}");
        assert_eq!(
            rebuild_result["indices_built"],
            json!(true),
            "{rebuild_result}"
        );

        let status_resp = rebuilt.call_tool("knowledge_status", json!({}));
        let status = structured(&status_resp, "knowledge_status (rebuilt-from-dump)");
        assert_eq!(
            status["entity_count"], expected["entity_count"],
            "rebuilt-from-dump entity_count must exactly match the original: {status}"
        );
        assert_eq!(
            status["relationship_count"], expected["relationship_count"],
            "rebuilt-from-dump relationship_count must exactly match the original: {status}"
        );
        assert_eq!(
            status["episode_count"], expected["episode_count"],
            "rebuilt-from-dump episode_count must exactly match the original: {status}"
        );

        let list_entities_resp = rebuilt.call_tool(
            "knowledge_list_entities",
            json!({"group_ids": [group_id.clone()], "num_results": entity_total + 100}),
        );
        let list_entities_result = structured(
            &list_entities_resp,
            "knowledge_list_entities (rebuilt-from-dump)",
        );
        assert_exact_uuid_enumeration(
            list_entities_result,
            "nodes",
            entity_total,
            "knowledge_list_entities (rebuilt-from-dump)",
        );
        let rebuilt_entities_by_uuid: std::collections::HashMap<String, String> =
            list_entities_result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    (
                        n["uuid"].as_str().unwrap().to_string(),
                        n["name"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
        for (uuid, name) in &sample_entities {
            let rebuilt_name = rebuilt_entities_by_uuid.get(uuid).unwrap_or_else(|| {
                panic!("sample entity {uuid} ({name}) missing from rebuilt-from-dump graph")
            });
            assert_eq!(
                rebuilt_name, name,
                "sample entity {uuid} name mismatch after dump→rebuild round trip"
            );
        }

        let episodes_resp = rebuilt.call_tool(
            "knowledge_get_episodes",
            json!({"group_id": group_id.clone(), "last_n": episode_total + 50}),
        );
        let episodes_result =
            structured(&episodes_resp, "knowledge_get_episodes (rebuilt-from-dump)");
        assert_exact_uuid_enumeration(
            episodes_result,
            "episodes",
            episode_total,
            "knowledge_get_episodes (rebuilt-from-dump)",
        );
        let rebuilt_episode_uuids = as_uuid_set(episodes_result, "episodes");
        for uuid in &sample_episode_uuids {
            assert!(
                rebuilt_episode_uuids.contains(uuid),
                "sample episode {uuid} missing from rebuilt-from-dump graph"
            );
        }

        let sample_edge_uuids: Vec<Value> =
            sample_edges.iter().map(|s| s["uuid"].clone()).collect();
        let edges_by_uuids_resp = rebuilt.call_tool(
            "knowledge_get_edges_by_uuids",
            json!({"uuids": sample_edge_uuids}),
        );
        let edges_by_uuids_result = structured(
            &edges_by_uuids_resp,
            "knowledge_get_edges_by_uuids (rebuilt-from-dump)",
        );
        let rebuilt_edges_by_uuid: std::collections::HashMap<String, Value> = edges_by_uuids_result
            ["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["uuid"].as_str().unwrap().to_string(), e.clone()))
            .collect();
        for sample in &sample_edges {
            let uuid = sample["uuid"].as_str().unwrap();
            let actual = rebuilt_edges_by_uuid.get(uuid).unwrap_or_else(|| {
                panic!("sample edge {uuid} missing from rebuilt-from-dump graph")
            });
            assert_eq!(
                actual["relation_type"], sample["relation_type"],
                "sample edge {uuid} relation_type mismatch after dump→rebuild round trip"
            );
            assert_eq!(
                actual["fact"], sample["fact"],
                "sample edge {uuid} fact mismatch after dump→rebuild round trip"
            );
        }

        rebuilt.shutdown();
    }

    // ── User Story 2 (FR-004) + User Story 4 Scenario 1 (FR-005): full rebuild-with-progress,
    //    indices_built + immediate golden search, progress notifications observed ────────────
    {
        let fork = base.fork();
        let mut client = fork.spawn_reader(&embedder_url, &[ADMIN_READ_SCOPE]);
        client.initialize();

        let rebuild_resp = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us2-us4-full-rebuild",
            Duration::from_secs(600),
        );
        let rebuild_result = structured(&rebuild_resp, "knowledge_rebuild_from_wal (US2/US4)");
        assert_eq!(rebuild_result["success"], json!(true), "{rebuild_result}");
        assert_eq!(
            rebuild_result["indices_built"],
            json!(true),
            "full rebuild must report indices_built: true with no intervening \
             knowledge_build_indices call: {rebuild_result}"
        );
        assert!(
            client
                .stashed_notifications
                .iter()
                .any(|n| n["method"] == "notifications/progress"),
            "expected at least one progress notification for a full-fixture-scale rebuild with a \
             progress token supplied, got: {:?}",
            client.stashed_notifications
        );

        // No intervening knowledge_build_indices call between the rebuild above and this query.
        let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
        let q = &entity_queries[0];
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let find_resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
        );
        let find_result = structured(
            &find_resp,
            "knowledge_find_entities (post-rebuild, US2/US4)",
        );
        let actual_names: HashSet<String> = find_result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_string())
            .collect();
        let expected_names: Vec<String> = q["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            expected_names.iter().any(|n| actual_names.contains(n)),
            "post-rebuild golden query {query:?} expected overlap with {expected_names:?}, got \
             {actual_names:?}"
        );

        client.shutdown();
    }

    // ── User Story 4 Scenario 2 (FR-005): no progress token ⇒ no progress notifications ───
    // Deliberately decoupled from the real-fixture-scale rebuild above: a group_id-scoped dump
    // against a group absent from the fixture (the natural "near-empty rebuild target"
    // candidate) never writes a single WAL file — `WalWriter` only ever creates a file lazily on
    // its first write (see `wal.rs`), and a zero-row dump never calls `write` at all — so
    // `knowledge_rebuild_from_wal` against it fails fast with "No WAL files found" rather than
    // completing. A tiny synthetic single-empty-file WAL (mirroring `mcp_progress.rs`'s own
    // technique) exercises the exact same gating logic (`wants_progress = progress_token.is_some()
    // && is_streaming_method(...)`) at negligible cost, which is what this scenario actually
    // guards — the *token*, not the fixture's scale, gates notification emission.
    {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        std::fs::write(wal_dir.join("0000000000000-seed.jsonl"), "").unwrap();
        // #429: without a generation stamp, `main.rs`'s `backfill_applied_seq_if_absent` (which
        // runs at every startup) records `applied_seq: Some(0)` with `generation: None` for this
        // fresh DB, and `handle_rebuild_from_wal`'s unknown-generation guard then refuses to
        // replay — mirrors `common::real_corpus::seed_real_corpus_workspace`'s identical
        // workaround one function away in this crate.
        lcg_core::wal_generation::ensure_generation(&wal_dir)
            .expect("mint a generation for the synthetic single-empty-file WAL");
        let db_path = dir.path().join("no-token-rebuild.db");

        let mut client = spawn_fresh(&db_path, &wal_dir, &embedder_url, &["--scope=admin"]);
        client.initialize();

        let resp = client.call_tool("knowledge_rebuild_from_wal", json!({}));
        assert!(
            resp["result"]["isError"].as_bool() != Some(true),
            "rebuild without a progress token should still succeed: {resp:?}"
        );
        assert!(
            !client
                .stashed_notifications
                .iter()
                .any(|n| n["method"] == "notifications/progress"),
            "expected zero progress notifications when no progress token was supplied, got: {:?}",
            client.stashed_notifications
        );

        client.shutdown();
    }

    // ── User Story 5 (FR-006): checkpoint leaves a coherent, still-rebuildable database ────
    {
        let fork = base.fork();
        let mut client = fork.spawn_reader(&embedder_url, &[ADMIN_READ_SCOPE]);
        client.initialize();

        let checkpoint_resp = client.call_tool("knowledge_prepare_checkpoint", json!({}));
        let checkpoint_result = structured(&checkpoint_resp, "knowledge_prepare_checkpoint");
        assert_eq!(
            checkpoint_result["success"],
            json!(true),
            "{checkpoint_result}"
        );

        let status_resp = client.call_tool("knowledge_status", json!({}));
        let status = structured(&status_resp, "knowledge_status (post-checkpoint)");
        assert_eq!(status["entity_count"], expected["entity_count"], "{status}");
        assert_eq!(
            status["relationship_count"], expected["relationship_count"],
            "{status}"
        );
        assert_eq!(
            status["episode_count"], expected["episode_count"],
            "{status}"
        );

        let rebuild_resp = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us5-post-checkpoint-rebuild",
            Duration::from_secs(600),
        );
        let rebuild_result = structured(
            &rebuild_resp,
            "knowledge_rebuild_from_wal (post-checkpoint)",
        );
        assert_eq!(rebuild_result["success"], json!(true), "{rebuild_result}");
        assert_eq!(
            rebuild_result["indices_built"],
            json!(true),
            "{rebuild_result}"
        );

        let status2_resp = client.call_tool("knowledge_status", json!({}));
        let status2 = structured(&status2_resp, "knowledge_status (post-checkpoint rebuild)");
        assert_eq!(
            status2["entity_count"], expected["entity_count"],
            "{status2}"
        );
        assert_eq!(
            status2["relationship_count"], expected["relationship_count"],
            "{status2}"
        );
        assert_eq!(
            status2["episode_count"], expected["episode_count"],
            "{status2}"
        );

        client.shutdown();
    }

    // ── User Story 6 (FR-010): knowledge_build_indices restores search on an index-less graph ─
    // No WAL replay is involved here — the fork is already fully populated and already
    // eagerly-indexed at startup (ADR-0036), so a column-drop (the existing synthetic-scale
    // technique in `handlers_wal_admin.rs`) can't be reused: every indexed column is also written
    // by WAL replay, and this suite must stay replay-safe. Instead, `knowledge_query_cypher`
    // temporarily renames the `Entity` table away (`ALTER TABLE ... RENAME TO`, validated against
    // a small synthetic DB in Plan's Task 1) — `CREATE_VECTOR_INDEX('Entity', ...)` then fails
    // genuinely ("table does not exist", not lbug's "already exists" tolerance), which is exactly
    // `handle_build_indices`'s tracked-failure path. Renaming back is a pure catalog-metadata
    // operation — no row data or other tables' internal references are disturbed.
    {
        let fork = base.fork();
        let mut client = fork.spawn_reader(&embedder_url, &[ADMIN_READ_CYPHER_SCOPE]);
        client.initialize();

        let status0_resp = client.call_tool("knowledge_status", json!({}));
        let status0 = structured(&status0_resp, "knowledge_status (US6 baseline)");
        assert_eq!(
            status0["indices_built"],
            json!(true),
            "fork must start with indices already built (eager startup build): {status0}"
        );

        let rename_away_resp = client.call_tool(
            "knowledge_query_cypher",
            json!({"query": "ALTER TABLE Entity RENAME TO EntityTmp"}),
        );
        assert!(
            rename_away_resp["result"]["isError"].as_bool() != Some(true),
            "ALTER TABLE Entity RENAME TO EntityTmp should succeed: {rename_away_resp:?}"
        );

        let build_fail_resp = client.call_tool("knowledge_build_indices", json!({}));
        assert_eq!(
            build_fail_resp["result"]["isError"],
            json!(true),
            "knowledge_build_indices must genuinely fail while Entity is renamed away: \
             {build_fail_resp:?}"
        );

        let status1_resp = client.call_tool("knowledge_status", json!({}));
        let status1 = structured(&status1_resp, "knowledge_status (US6 index-less)");
        assert_eq!(
            status1["indices_built"],
            json!(false),
            "indices_built must accurately report false, not stale-true: {status1}"
        );

        let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
        let query = entity_queries[0]["query"].as_str().unwrap();
        let degraded_search_resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": 10}),
        );
        let degraded_is_error = degraded_search_resp["result"]["isError"].as_bool() == Some(true);
        let degraded_empty = degraded_search_resp["result"]["structuredContent"]["nodes"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true);
        assert!(
            degraded_is_error || degraded_empty,
            "expected knowledge_find_entities to fail or return no results while Entity is \
             renamed away: {degraded_search_resp:?}"
        );

        let rename_back_resp = client.call_tool(
            "knowledge_query_cypher",
            json!({"query": "ALTER TABLE EntityTmp RENAME TO Entity"}),
        );
        assert!(
            rename_back_resp["result"]["isError"].as_bool() != Some(true),
            "ALTER TABLE EntityTmp RENAME TO Entity should succeed: {rename_back_resp:?}"
        );

        let build_ok_resp = client.call_tool("knowledge_build_indices", json!({}));
        let build_ok_result = structured(&build_ok_resp, "knowledge_build_indices (US6 restore)");
        assert_eq!(
            build_ok_result["indices_built"],
            json!(true),
            "{build_ok_result}"
        );

        let status2_resp = client.call_tool("knowledge_status", json!({}));
        let status2 = structured(&status2_resp, "knowledge_status (US6 restored)");
        assert_eq!(status2["indices_built"], json!(true), "{status2}");
        assert_eq!(
            status2["entity_count"], expected["entity_count"],
            "entity data must survive the rename round trip untouched: {status2}"
        );

        let restored_search_resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": 10}),
        );
        let restored_result = structured(
            &restored_search_resp,
            "knowledge_find_entities (US6 restored)",
        );
        let restored_names: HashSet<String> = restored_result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_string())
            .collect();
        let expected_names: Vec<String> = entity_queries[0]["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            expected_names.iter().any(|n| restored_names.contains(n)),
            "restored golden query {query:?} expected overlap with {expected_names:?}, got \
             {restored_names:?}"
        );

        client.shutdown();
    }

    // FR-015: verified environmentally (see this file's module doc comment) — no
    // ANTHROPIC_API_KEY is set and every --embedder-http URL above points only at this suite's
    // own loopback stub.
}
