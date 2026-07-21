//! Static registry of MCP tools derived from the `knowledge_*` dispatch methods in
//! `lcg_core::handlers` (FR-002). Tool name == IPC method name, verbatim, so the registry
//! stays directly auditable against `handlers.rs`'s `match` arms — nothing here is derived by
//! reflection, and adding a new `knowledge_*` method requires a matching new entry here.
//!
//! Schemas are plain `serde_json::Value` literals rather than per-tool `schemars`-derived
//! structs: tool-call arguments pass straight through to `handlers::dispatch` as a raw
//! `Value` (FR-003), so there is no typed deserialization step that would justify ~33 throwaway
//! structs. This is the single source of truth FR-002 requires; there is no second,
//! hand-maintained schema anywhere else.
//!
//! Descriptions and schemas are authored from the issue spec's FRs and from each handler's
//! actual parameter extraction in `handlers.rs`, per the spec's own Assumptions fallback: the
//! app's zod tool defs live in a separate closed-source repo not reachable from this
//! environment, so SC-006's "verified against the zod defs" comparison is a manual step (see
//! the PR description) rather than one this registry can automate.

use serde_json::{json, Value};

use crate::mcp::scope::Scope;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub scope: Scope,
    pub input_schema: fn() -> Value,
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn group_ids_prop() -> Value {
    json!({
        "type": "array",
        "items": {"type": "string"},
        "description": "Optional group IDs to scope the operation to. Omit for all groups \
                         (or the default group, depending on the tool)."
    })
}

/// The full, ordered registry — one entry per `knowledge_*` dispatch method (33 total),
/// matching FR-004's scope table exactly.
pub fn registry() -> Vec<ToolSpec> {
    vec![
        // ── read (14) ──────────────────────────────────────────────────────────────
        ToolSpec {
            name: "knowledge_status",
            description: "Get knowledge graph status: entity/episode/relationship counts, \
                           embedding config, WAL state, ontology summary, and whether search \
                           indices are built.",
            scope: Scope::Read,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_find_entities",
            description: "Hybrid (full-text + vector) search for entities matching a query.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search text."},
                        "group_ids": group_ids_prop(),
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 10,
                            "description": "Maximum number of entities to return."
                        }
                    },
                    "required": ["query"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_find_relationships",
            description: "Hybrid (full-text + vector) search for relationships (facts) \
                           matching a query.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search text."},
                        "group_ids": group_ids_prop(),
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 10,
                            "description": "Maximum number of relationships to return."
                        }
                    },
                    "required": ["query"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_episodes",
            description: "Retrieve the most recent episodes (ingested source documents) for \
                           a group.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "group_id": {
                            "type": "string", "default": "liminis",
                            "description": "Group to retrieve episodes from."
                        },
                        "last_n": {
                            "type": "integer", "minimum": 1, "default": 50,
                            "description": "Number of most recent episodes to return."
                        }
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_nodes_by_group",
            description: "List all entity nodes belonging to the given group IDs.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {"group_ids": group_ids_prop()},
                    "required": ["group_ids"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_edges_by_group",
            description: "List all relationship edges belonging to the given group IDs.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {"group_ids": group_ids_prop()},
                    "required": ["group_ids"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_edges_by_uuids",
            description: "Fetch relationship edges by their UUIDs.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "uuids": {
                            "type": "array", "items": {"type": "string"},
                            "description": "Edge UUIDs to fetch."
                        }
                    },
                    "required": ["uuids"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_search_passages",
            description: "Semantic passage search over ingested episode text, returning \
                           scored text snippets.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search text (required, non-empty)."},
                        "num_results": {
                            "type": "integer", "minimum": 1, "maximum": 100, "default": 10,
                            "description": "Maximum number of passages to return (clamped 1-100)."
                        },
                        "min_score": {
                            "type": "number", "minimum": 0.0, "maximum": 1.0, "default": 0.5,
                            "description": "Minimum similarity score (clamped 0.0-1.0)."
                        },
                        "group_ids": group_ids_prop()
                    },
                    "required": ["query"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_list_entities",
            description: "List entity nodes, optionally scoped to specific group IDs, with \
                           episode provenance attached.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 500,
                            "description": "Maximum number of entities to return."
                        },
                        "group_ids": group_ids_prop()
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_list_relationships",
            description: "List relationship edges (facts), optionally scoped to specific \
                           group IDs, with episode provenance attached.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 1000,
                            "description": "Maximum number of relationships to return."
                        },
                        "group_ids": group_ids_prop()
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_entity_neighbors",
            description: "Get the immediate graph neighborhood (connected edges and nodes) \
                           of an entity.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "entity_uuid": {"type": "string", "description": "Center entity UUID (required)."},
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 50,
                            "description": "Maximum number of neighbors to return."
                        },
                        "group_ids": group_ids_prop()
                    },
                    "required": ["entity_uuid"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_get_entities_by_source",
            description: "List entities that were extracted from a given source document.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Source identifier (required, non-empty)."},
                        "num_results": {
                            "type": "integer", "minimum": 1, "default": 100,
                            "description": "Maximum number of entities to return."
                        },
                        "group_ids": group_ids_prop()
                    },
                    "required": ["source"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_rebuild_status",
            description: "Poll the status of a background `knowledge_rebuild_from_wal` job \
                           by job ID.",
            scope: Scope::Read,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "job_id": {"type": "string", "description": "Job ID returned by knowledge_rebuild_from_wal."}
                    },
                    "required": ["job_id"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_validate_corrections",
            description: "Validate the workspace's `knowledge-corrections.yaml` file against \
                           the current graph without applying anything.",
            scope: Scope::Read,
            input_schema: empty_schema,
        },
        // ── write (11) ────────────────────────────────────────────────────────────
        ToolSpec {
            name: "knowledge_process_chunk",
            description: "Ingest a text chunk as an episode: extracts entities/relationships \
                           and adds them to the graph.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "chunk_text": {"type": "string", "description": "Chunk text (required, non-empty)."},
                        "chunk_id": {"type": "string", "description": "Stable ID for this chunk (required, non-empty)."},
                        "source_file": {"type": "string", "description": "Source file path or identifier (required, non-empty)."},
                        "group_id": {"type": "string", "default": "liminis"},
                        "reference_time": {
                            "type": "string", "format": "date-time",
                            "description": "ISO 8601 timestamp; defaults to now."
                        }
                    },
                    "required": ["chunk_text", "chunk_id", "source_file"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_add_episode",
            description: "Add an episode (a piece of source content) to the graph, extracting \
                           entities and relationships from it.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Episode name."},
                        "episode_body": {"type": "string", "description": "Episode content."},
                        "source": {"type": "string", "default": "text", "description": "Source type (e.g. \"text\", \"json\")."},
                        "source_description": {"type": "string"},
                        "reference_time": {"type": "string", "format": "date-time"},
                        "group_id": {"type": "string", "default": "liminis"}
                    },
                    "required": ["name", "episode_body"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_delete_episode",
            description: "Delete a single episode by UUID. Entities extracted solely from it \
                           become orphaned (not deleted).",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "episode_uuid": {"type": "string", "description": "Episode UUID (required)."}
                    },
                    "required": ["episode_uuid"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_delete_by_source",
            description: "Delete all episodes ingested from a given source file. Orphaned \
                           entities remain in the graph.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "source_file": {"type": "string", "description": "Source file to delete episodes for (required, non-empty)."},
                        "group_ids": {
                            "type": "array", "items": {"type": "string"},
                            "description": "Restrict deletion to these groups. Omit to delete across all groups."
                        }
                    },
                    "required": ["source_file"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_delete_chunk_episode",
            description: "Delete all episodes for a given chunk ID. Orphaned entities remain \
                           in the graph.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "chunk_id": {"type": "string", "description": "Chunk ID to delete episodes for (required, non-empty)."},
                        "group_ids": {
                            "type": "array", "items": {"type": "string"},
                            "description": "Restrict deletion to these groups. Omit to delete across all groups."
                        }
                    },
                    "required": ["chunk_id"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_clear_all",
            description: "Irreversibly delete the entire graph and reinitialize an empty \
                           schema. Requires explicit confirmation.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "description": "Must be true, or the call is rejected. Confirms the caller intends to destroy all graph data."
                        },
                        "preserve_wal": {
                            "type": "boolean", "default": false,
                            "description": "If true, keep the application WAL so knowledge_rebuild_from_wal can replay it afterward."
                        }
                    },
                    "required": ["confirm"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_apply_corrections",
            description: "Apply the workspace's `knowledge-corrections.yaml` file to the graph.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview without writing."}
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_merge_entities",
            description: "Merge one or more alias entities into a canonical entity, rewriting \
                           and deduplicating their edges.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "canonical_uuid": {"type": "string", "description": "UUID of the entity to merge into."},
                        "canonical_name": {"type": "string", "description": "Name of the entity to merge into (alternative to canonical_uuid)."},
                        "alias_uuids": {"type": "array", "items": {"type": "string"}, "description": "UUIDs of entities to merge away."},
                        "alias_names": {"type": "array", "items": {"type": "string"}, "description": "Names of entities to merge away."},
                        "merge_all_by_name": {
                            "type": "boolean", "default": false,
                            "description": "If true, merge all entities sharing canonical_name as aliases."
                        },
                        "group_id": {"type": "string", "default": "liminis"},
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview the merge plan without writing."}
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_reprocess_entity_types",
            description: "Reclassify entity types via the configured extraction LLM, e.g. \
                           after an ontology change.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "group_id": {"type": "string", "default": "liminis"},
                        "scope": {
                            "type": "string", "enum": ["untyped", "off_ontology", "all"], "default": "untyped",
                            "description": "Which entities to reclassify: only untyped ones, only those outside the ontology, or all."
                        },
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview the reclassification plan without writing."}
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_canonicalize_relations",
            description: "Canonicalize relationship types against the workspace ontology's \
                           declared relation_types. Supports MCP progress notifications when \
                           called with a progress token.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview without writing."},
                        "embedding_threshold": {
                            "type": "number",
                            "description": "Optional similarity threshold override for matching relation types."
                        }
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_backfill_relation_types",
            description: "Backfill missing relation_type values on existing edges. Supports \
                           MCP progress notifications when called with a progress token.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview without writing."}
                    }
                })
            },
        },
        // ── cypher (1) — arbitrary query/mutation power scope ────────────────────────
        ToolSpec {
            name: "knowledge_query_cypher",
            description: "Execute raw Cypher against the graph. Can perform arbitrary reads or \
                           mutations and bypasses the WAL/embedding invariants that structured \
                           write tools maintain — use with caution.",
            scope: Scope::Cypher,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Raw Cypher query text (required)."}
                    },
                    "required": ["query"]
                })
            },
        },
        // ── admin (7) — WAL/lifecycle/recovery/index maintenance ─────────────────────
        ToolSpec {
            name: "knowledge_dump_wal",
            description: "Snapshot the current graph contents into a fresh compacted WAL \
                           directory.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "group_id": {"type": "string", "description": "Restrict the dump to a single group. Omit to dump all groups."},
                        "target_dir": {
                            "type": "string",
                            "description": "Output directory. Must not exist or must be empty. Defaults to <workspace>/.lcg/wal-compacted/."
                        }
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_prepare_checkpoint",
            description: "Rotate/flush the live WAL writer so all pending mutations are on \
                           disk before a checkpoint or backup.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_rebuild_from_wal",
            description: "Rebuild the graph by replaying application WAL files, optionally \
                           from a given sequence number. Supports MCP progress notifications \
                           when called with a progress token.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "from_seq": {
                            "type": "integer", "minimum": 0, "default": 0,
                            "description": "Replay starting from this WAL sequence number."
                        },
                        "dry_run": {
                            "type": "boolean", "default": false,
                            "description": "Compute replay statistics without writing or touching indices."
                        }
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_recover",
            description: "Run a single degraded-mode recovery strategy against a corrupt or \
                           unavailable database.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "strategy": {
                            "type": "string",
                            "enum": ["drop_lbug_wal", "rebuild_from_workspace_wal", "restore_from_backup"],
                            "description": "Recovery strategy to run (required)."
                        }
                    },
                    "required": ["strategy"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_recover_full",
            description: "Run the full autonomous recovery sequence (checkpoint-drop → \
                           episode-cursor resume-replay → reindex). Idempotent: a no-op if the \
                           DB is already healthy.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_close",
            description: "Gracefully shut down the knowledge graph service. In standalone MCP \
                           mode, closes only this MCP process's own DB connection. In attached \
                           mode (only advertised with --allow-remote-close), forwards the \
                           shutdown to the remote service.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_build_indices",
            description: "Build the full-text and vector search indices over the current \
                           graph contents.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
    ]
}

/// Names of the three streaming methods that emit MCP progress notifications (FR-007).
pub fn is_streaming_method(name: &str) -> bool {
    matches!(
        name,
        "knowledge_rebuild_from_wal"
            | "knowledge_canonicalize_relations"
            | "knowledge_backfill_relation_types"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_has_33_unique_tools() {
        let r = registry();
        assert_eq!(r.len(), 33);
        let names: HashSet<&str> = r.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 33, "tool names must be unique");
    }

    #[test]
    fn scope_bucket_sizes_match_fr_004_table() {
        let r = registry();
        let count = |s: Scope| r.iter().filter(|t| t.scope == s).count();
        assert_eq!(count(Scope::Read), 14);
        assert_eq!(count(Scope::Write), 11);
        assert_eq!(count(Scope::Cypher), 1);
        assert_eq!(count(Scope::Admin), 7);
    }

    #[test]
    fn cypher_scope_is_exactly_query_cypher() {
        let r = registry();
        let cypher_tools: Vec<&str> = r
            .iter()
            .filter(|t| t.scope == Scope::Cypher)
            .map(|t| t.name)
            .collect();
        assert_eq!(cypher_tools, vec!["knowledge_query_cypher"]);
    }

    #[test]
    fn every_schema_is_a_valid_object_schema() {
        for tool in registry() {
            let schema = (tool.input_schema)();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool {} schema must have type object",
                tool.name
            );
            assert!(
                schema.get("properties").is_some(),
                "tool {} schema must have properties",
                tool.name
            );
        }
    }

    #[test]
    fn streaming_methods_match_admin_and_write_tools() {
        assert!(is_streaming_method("knowledge_rebuild_from_wal"));
        assert!(is_streaming_method("knowledge_canonicalize_relations"));
        assert!(is_streaming_method("knowledge_backfill_relation_types"));
        assert!(!is_streaming_method("knowledge_status"));
    }
}
