//! Static registry of MCP tools derived from the `knowledge_*` dispatch methods in
//! `lcg_core::handlers` (FR-002). Tool name == IPC method name, verbatim, so the registry
//! stays directly auditable against `handlers.rs`'s `match` arms — nothing here is derived by
//! reflection, and adding a new `knowledge_*` method requires a matching new entry here.
//!
//! Schemas are plain `serde_json::Value` literals rather than per-tool `schemars`-derived
//! structs: tool-call arguments pass straight through to `handlers::dispatch` as a raw
//! `Value` (FR-003), so there is no typed deserialization step that would justify ~39 throwaway
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

/// The full, ordered registry — one entry per `knowledge_*` dispatch method (42 total),
/// matching FR-004's scope table exactly.
pub fn registry() -> Vec<ToolSpec> {
    vec![
        // ── read (14) ──────────────────────────────────────────────────────────────
        ToolSpec {
            name: "knowledge_status",
            description: "Get knowledge graph status: entity/episode/relationship counts, \
                           embedding config, WAL state, ontology summary, and whether search \
                           indices are built. Returns a status object (not a JSON-RPC error) \
                           when the database is open but a core table is missing — check the \
                           `queryable` field (and `reason` when false) to distinguish that state \
                           from a genuinely empty graph, whose counts read as 0 rather than \
                           null. Other query failures still surface as JSON-RPC errors.",
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
        // ── write (15) ────────────────────────────────────────────────────────────
        ToolSpec {
            name: "knowledge_process_chunk",
            description: "Ingest a text chunk as an episode: extracts entities/relationships \
                           and adds them to the graph. The result reports \
                           `edges_dropped_unresolvable`: extracted edges discarded because an \
                           endpoint matched no entity in this chunk or in the graph. A nonzero \
                           count means facts stated in this chunk were not written.",
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
            description: "DEPRECATED — does not classify against the ontology. For each edge \
                           with a null/empty relation_type, this mints a pseudo-type by \
                           uppercasing and underscore-joining the first few words of the edge's \
                           fact sentence (e.g. THE_SPECIFICATION_DOCUMENT_DEFINES), producing \
                           near-unique labels rather than a real taxonomy. Running it pollutes \
                           the relation_type space and is only reversible by re-nulling the \
                           field. Use knowledge_reprocess_relation_types with scope: \"untyped\" \
                           instead — it performs genuine fact-based classification against the \
                           ontology's declared relation types, with honest UNCLASSIFIED \
                           abstention. Retained for backward compatibility; supports MCP \
                           progress notifications when called with a progress token.",
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
            name: "knowledge_reprocess_relation_types",
            description: "Reclassify relation types via the configured extraction LLM, using \
                           each edge's fact against the ontology's declared relation types. \
                           Honestly abstains to UNCLASSIFIED rather than force-assigning the \
                           nearest type. Successful dry-run (`would_reclassify_count`, `plan`) and \
                           apply (`reclassified_count`, `unchanged_count`) responses include a \
                           `breakdown` object mapping each relation type (including UNCLASSIFIED) \
                           to the count of relations that would be written in dry-run mode or \
                           were written in apply mode, so callers can see classification quality \
                           without a second dry-run pass. Supports MCP progress notifications \
                           when called with a progress token.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "group_id": {"type": "string", "default": "liminis"},
                        "scope": {
                            "type": "string", "enum": ["untyped", "off_ontology", "all"], "default": "untyped",
                            "description": "Which edges to reclassify: only untyped ones, only those outside the ontology, or all."
                        },
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview the reclassification plan without writing."}
                    }
                })
            },
        },
        ToolSpec {
            name: "knowledge_add_cross_group_edge",
            description: "Create an edge whose endpoint(s) may live in a group other than the \
                           edge's own `group_id` (issue #369 hub/layer-graph topology). Each \
                           endpoint is either `{\"uuid\": \"...\"}` for one already known to \
                           live in the edge's own group, or `{\"source_group_id\": \"...\", \
                           \"endpoint_name\": \"...\"}` for a foreign endpoint, which is \
                           resolved by name against that group and given resolvable pointer \
                           fields (`source_group_id`, `endpoint_name`, `resolved_uuid`, \
                           `bound_at_seq`, `binding_state`) instead of a bare UUID FK. A foreign \
                           endpoint that doesn't currently resolve is retained as `unbound` \
                           rather than dropped — the edge is still created, just missing that \
                           hop until a `knowledge_rebind_pointers` pass resolves it. A bare \
                           `uuid` endpoint that turns out to belong to a different group than \
                           the edge is rejected before any write.",
            scope: Scope::Write,
            input_schema: || {
                let endpoint_schema = || {
                    json!({
                        "oneOf": [
                            {
                                "type": "object",
                                "description": "An endpoint already known to live in the edge's own group_id.",
                                "properties": {"uuid": {"type": "string"}},
                                "required": ["uuid"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "description": "A foreign endpoint, resolved by name against source_group_id.",
                                "properties": {
                                    "source_group_id": {"type": "string"},
                                    "endpoint_name": {"type": "string"}
                                },
                                "required": ["source_group_id", "endpoint_name"],
                                "additionalProperties": false
                            }
                        ]
                    })
                };
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Relation name, e.g. RELATES_TO subtype label."},
                        "source": endpoint_schema(),
                        "target": endpoint_schema(),
                        "group_id": {"type": "string", "default": "liminis", "description": "The edge's own (layer) group_id."},
                        "fact": {"type": "string", "description": "Natural-language fact text for the edge."},
                        "valid_at": {"type": "string", "description": "Optional ISO-8601 timestamp this fact became true."},
                        "relation_type": {"type": "string", "description": "Optional ontology relation type."}
                    },
                    "required": ["name", "source", "target", "fact"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_assert_entity",
            description: "Directly create or update a single entity by name (or by explicit \
                           entity_uuid) within group_id (issue #379) — for an agent that already \
                           knows the fact and wants to record it without a prose round-trip \
                           through episode extraction. Without entity_uuid, resolves by exact \
                           (case-insensitive, whitespace-normalized) name match within group_id \
                           — no embedding-similarity fuzzy matching — and updates that entity in \
                           place if found, or creates a new one if not; repeated calls with the \
                           same name/group_id are idempotent and always return the same \
                           entity_uuid. With entity_uuid, performs a strict group-scoped lookup \
                           only — never a fallback that mints a new entity under a caller-chosen \
                           UUID, and the call fails if that UUID doesn't exist in group_id. If \
                           resolution (by either path) lands on a Merged tombstone, the \
                           assertion forwards through merged_into to the canonical entity and \
                           updates the canonical instead. `summary` and `attributes` are always \
                           fully replaced on update, like `labels` — a re-assert that omits \
                           `summary` clears any previously-set value, not a partial merge. If the \
                           configured embedder is unavailable, the call still succeeds with a \
                           zero-vector name_embedding and a non-null `embedding_warning` in the \
                           response.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The entity's name (required)."},
                        "entity_uuid": {"type": "string", "description": "Optional: update this exact entity by UUID (strict group-scoped lookup, no create fallback) instead of resolving by name."},
                        "group_id": {"type": "string", "default": "liminis"},
                        "labels": {"type": "array", "items": {"type": "string"}, "default": ["Entity"], "description": "Defaults to [\"Entity\"] when omitted or empty."},
                        "summary": {"type": "string", "default": "", "description": "Fully replaces any prior summary on update; defaults to \"\" when omitted."},
                        "attributes": {"type": "object", "description": "Arbitrary JSON object, stored JSON-serialized. Fully replaces the prior value on update."}
                    },
                    "required": ["name"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_assert_relationship",
            description: "Directly create or update a single directed edge between two entities \
                           resolved by name, both strictly within this call's own group_id \
                           (issue #379) — for an agent that already knows the relationship and \
                           wants to record it without a prose round-trip through episode \
                           extraction. source_name/target_name are resolved by exact \
                           (case-insensitive, whitespace-normalized) match within group_id only \
                           — including forwarding through a Merged tombstone to its canonical, \
                           same as knowledge_assert_entity — and NEVER fall back to searching \
                           another group; if either name doesn't resolve in group_id, the call \
                           fails naming knowledge_add_cross_group_edge as the tool to use for \
                           cross-group connections instead. The upsert match is \
                           (source, predicate, target, group_id) — all four components, so \
                           asserting an edge in one group_id can never match, update, or \
                           overwrite a same-named edge between the same endpoints in a different \
                           group_id. Repeated calls with the same source/predicate/target/ \
                           group_id are idempotent: the same edge is updated in place, never \
                           duplicated. `fact` defaults to \"<source_name> <predicate> \
                           <target_name>\" when omitted. `attributes`, `valid_at`, and \
                           `relation_type` are always fully replaced on update — a re-assert \
                           that omits one of them clears any previously-set value, not a \
                           partial merge. `valid_at` accepts RFC-3339 or lbug's \
                           space-delimited read-back format and is rejected cleanly (not passed \
                           through to a Binder exception) if it matches neither. If the \
                           configured embedder is unavailable, the call still succeeds with a \
                           zero-vector fact_embedding and a non-null `embedding_warning` in the \
                           response.",
            scope: Scope::Write,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "source_name": {"type": "string", "description": "Source entity name, resolved within group_id (required)."},
                        "target_name": {"type": "string", "description": "Target entity name, resolved within group_id (required)."},
                        "predicate": {"type": "string", "description": "The edge's identity label — participates in the (source, predicate, target, group_id) upsert match (required)."},
                        "group_id": {"type": "string", "default": "liminis"},
                        "fact": {"type": "string", "description": "Natural-language fact text. Defaults to \"<source_name> <predicate> <target_name>\" when omitted."},
                        "attributes": {"type": "object", "description": "Arbitrary JSON object, stored JSON-serialized. Fully replaces the prior value on update."},
                        "valid_at": {"type": "string", "description": "Optional timestamp this fact became true — RFC-3339 or lbug's space-delimited read-back format. Fully replaces the prior value on update; omitting it on a re-assert clears any previously-set value, like attributes."},
                        "relation_type": {"type": "string", "description": "Optional ontology relation type. Fully replaces the prior value on update; omitting it on a re-assert clears any previously-set value, like attributes."}
                    },
                    "required": ["source_name", "target_name", "predicate"]
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
        // ── admin (12) — WAL/lifecycle/recovery/index maintenance ────────────────────
        ToolSpec {
            name: "knowledge_dump_wal",
            description: "Snapshot the current graph contents into a fresh compacted WAL \
                           directory. The output directory starts with no checkpoints — any \
                           `knowledge_wal_mark_*` checkpoints recorded against the source WAL \
                           directory are NOT carried forward, since dump_wal renumbers \
                           sequence numbers and a copied checkpoint's seq would be meaningless \
                           against the new numbering.",
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
                           disk before an external filesystem checkpoint or backup. Unrelated \
                           to knowledge_wal_mark_create/_list/_delete: this is a disk-flush \
                           operation, not a named WAL position — the two features share the \
                           word \"checkpoint\" by coincidence, not by relation.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_wal_mark_create",
            description: "Record a new named, retained WAL position — 'this graph was \
                           known-good here' — at the database's current applied_seq. Distinct \
                           from knowledge_prepare_checkpoint (a WAL flush/rotate operation, not \
                           a named position). O(1): does not scan or replay the WAL. Fails if \
                           the current position is unknown (applied_seq is null — resolve with \
                           a full knowledge_rebuild_from_wal first) or if `name` already \
                           identifies an active checkpoint. `name` must be 1-200 characters of \
                           [A-Za-z0-9_-], since it becomes a single directory name under \
                           .checkpoints/. The recorded `seq` is `null` for a genuinely \
                           fresh/empty graph, or an integer (WAL line, inclusive) otherwise — \
                           restore a `null` checkpoint with knowledge_clear_all, or an integer \
                           checkpoint with knowledge_rebuild_from_wal \
                           {from_seq: 0, to_seq: <seq>, force_clear: true}. Exactly-one-wins \
                           under concurrent create for the same name relies on exclusive file \
                           creation, which is a local-filesystem guarantee — not reliable on an \
                           NFS-mounted WAL directory.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Checkpoint name (1-200 chars of [A-Za-z0-9_-]). \
                                             Must not already identify an active checkpoint."
                        }
                    },
                    "required": ["name"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_wal_mark_list",
            description: "List every active (non-deleted) named WAL checkpoint, each with its \
                           `seq`, whether it is currently `reachable`, and the `wal_min_seq`/ \
                           `wal_max_seq` bounds of WAL content presently on disk. `reachable` \
                           requires BOTH `wal_min_seq == 0` (the WAL's own prefix has not been \
                           externally truncated, e.g. by routine retention deleting old WAL \
                           files) AND `seq <= wal_max_seq` — a checkpoint whose seq merely falls \
                           inside `[wal_min_seq, wal_max_seq]` is still reported unreachable if \
                           `wal_min_seq > 0`, since restoring it would silently omit everything \
                           before `wal_min_seq`. This does NOT detect a gap in the middle of \
                           that range, so `reachable: true` is a necessary, not sufficient, \
                           signal that a restore will succeed; use `wal_min_seq`/`wal_max_seq` \
                           to diagnose an unreachable checkpoint. Works even when the database \
                           is degraded or unavailable, since it only reads the WAL-directory \
                           checkpoint store.",
            scope: Scope::Admin,
            input_schema: empty_schema,
        },
        ToolSpec {
            name: "knowledge_wal_mark_delete",
            description: "Delete an active named WAL checkpoint, freeing its name for reuse by \
                           a later knowledge_wal_mark_create. Records a tombstone rather than \
                           rewriting history. Fails if `name` does not currently identify an \
                           active checkpoint (never created, or already deleted). Works even \
                           when the database is degraded or unavailable, since it only touches \
                           the WAL-directory checkpoint store.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Name of the active checkpoint to delete."
                        }
                    },
                    "required": ["name"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_rebuild_from_wal",
            description: "Rebuild the graph by replaying application WAL files, optionally \
                           from a given sequence number and/or up to a given sequence number. \
                           Supports MCP progress notifications when called with a progress \
                           token. A `from_seq: 0` (default) full rebuild fails fast with an \
                           explicit error if the database already contains data, rather than \
                           silently producing a duplicate-primary-key failure per node — pass \
                           `force_clear: true` to clear the database automatically before \
                           replaying, or clear it first with `knowledge_clear_all`. This check \
                           does not apply to `from_seq > 0` (incremental resume against an \
                           intentionally non-empty database). A bounded rebuild (`to_seq` set) \
                           is NOT durable: WAL entries beyond `to_seq` remain on disk, unapplied \
                           — a later unbounded rebuild, or a `from_seq` resume that covers the \
                           excluded range, will reapply everything that was excluded.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "from_seq": {
                            "type": "integer", "minimum": 0, "default": 0,
                            "description": "Replay starting from this WAL sequence number."
                        },
                        "to_seq": {
                            "type": "integer", "minimum": 0,
                            "description": "Inclusive upper bound: replay only lines with \
                                             seq <= to_seq. Omit for unbounded replay to the end \
                                             of the WAL (today's default behavior). Must not be \
                                             less than from_seq. Not durable: WAL entries beyond \
                                             to_seq remain on disk and will be reapplied by a \
                                             later unbounded replay or an overlapping from_seq \
                                             resume."
                        },
                        "dry_run": {
                            "type": "boolean", "default": false,
                            "description": "Compute replay statistics without writing or touching indices. \
                                             Still fails fast against a non-empty database on a from_seq: 0 \
                                             request, regardless of force_clear, since a dry run must never \
                                             mutate the database."
                        },
                        "force_clear": {
                            "type": "boolean", "default": false,
                            "description": "When true and from_seq is 0, clear the database before \
                                             replaying if it already contains data, instead of failing fast. \
                                             Ignored for from_seq > 0 and for dry_run (which always fails \
                                             fast on a non-empty database)."
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
        ToolSpec {
            name: "knowledge_rebind_pointers",
            description: "Re-resolve every cross-group pointer whose `source_group_id` matches \
                           the given group (issue #369) — run after that source group's \
                           purge/rehydrate refresh cycle so layer edges follow re-extracted \
                           entities, merges, and renames. Uses the same name-index resolution \
                           authority as pointer creation (ADR-0283), so results always agree \
                           with what a fresh lookup would find. Skips any pointer whose \
                           `bound_at_seq` is already at or past the current applied WAL \
                           position, which is what makes a second call with no intervening \
                           source change a no-op. A resolution that would create a self-loop or \
                           duplicate an existing directed edge invalidates the edge instead of \
                           writing a broken/redundant one, reusing the same handling \
                           `knowledge_merge_entities` uses.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "source_group_id": {"type": "string", "description": "The source group whose pointers should be re-resolved (required)."}
                    },
                    "required": ["source_group_id"]
                })
            },
        },
        ToolSpec {
            name: "knowledge_delete_by_group",
            description: "Purge ALL data for one or more group_ids: Entity nodes, Episodic \
                           nodes, and the RELATES_TO edges among them, leaving no orphans \
                           (issue #361). Supersedes the ad hoc `knowledge_query_cypher` \
                           `DELETE ... WHERE group_id = $g` workaround, which bypasses the WAL \
                           and embedding invariants the structured write tools maintain. Never \
                           deletes a RelatesToNode_ owned by a group outside the call, even when \
                           it deletes an Entity that node is attached to via a hop relationship \
                           — the foreign edge node survives, left `unbound` (issue #369's \
                           binding_state), so a later rehydration can re-bind it with \
                           knowledge_rebind_pointers. Purging a group_id with no data is a \
                           no-op success, not an error. Does not reset the applied WAL position \
                           (`applied_seq`), which is a DB-wide singleton, not per-group, until \
                           #378 lands. Pass `dry_run: true` to preview per-group entity/edge/ \
                           episode counts and the cross-group pointers that would become \
                           `unbound` (broken out by owning group_id) without mutating anything \
                           — `dry_run` takes precedence over `confirm` when both are set.",
            scope: Scope::Admin,
            input_schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "group_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 1,
                            "description": "Required, non-empty list of group_ids to purge \
                                             completely. Unlike other tools' optional group_ids \
                                             filter, this one is never defaulted — a destructive \
                                             admin op must never silently guess which group to \
                                             purge."
                        },
                        "confirm": {
                            "type": "boolean", "default": false,
                            "description": "Must be true to perform the real (mutating) purge. \
                                             Ignored when dry_run is true."
                        },
                        "dry_run": {
                            "type": "boolean", "default": false,
                            "description": "Preview the purge's counts without mutating \
                                             anything. Takes precedence over confirm."
                        }
                    },
                    "required": ["group_ids"]
                })
            },
        },
    ]
}

/// Returns the names from `schema`'s `"required"` array that are absent (or explicitly `null`)
/// in `params`. `handlers.rs`'s own param extraction is untouched by this issue and some
/// handlers silently fall back to an empty/default value instead of erroring when a field the
/// schema advertises as required is missing (e.g. `knowledge_delete_episode` would report
/// `"deleted"` without deleting anything) — this check makes the MCP layer honor its own
/// advertised contract (FR-008's "missing required argument" edge case) without touching the
/// out-of-scope core dispatch.
pub fn missing_required(schema: &Value, params: &Value) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    required
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|name| matches!(params.get(*name), None | Some(Value::Null)))
        .map(|s| s.to_string())
        .collect()
}

/// Names of the five streaming methods that emit MCP progress notifications (FR-007).
pub fn is_streaming_method(name: &str) -> bool {
    matches!(
        name,
        "knowledge_rebuild_from_wal"
            | "knowledge_canonicalize_relations"
            | "knowledge_backfill_relation_types"
            | "knowledge_reprocess_relation_types"
            | "knowledge_reprocess_entity_types"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_has_42_unique_tools() {
        let r = registry();
        assert_eq!(r.len(), 42);
        let names: HashSet<&str> = r.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 42, "tool names must be unique");
    }

    #[test]
    fn scope_bucket_sizes_match_fr_004_table() {
        let r = registry();
        let count = |s: Scope| r.iter().filter(|t| t.scope == s).count();
        assert_eq!(count(Scope::Read), 14);
        assert_eq!(count(Scope::Write), 15);
        assert_eq!(count(Scope::Cypher), 1);
        assert_eq!(count(Scope::Admin), 12);
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
    fn missing_required_reports_absent_and_null_fields_only() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {}, "b": {}, "c": {}},
            "required": ["a", "b"]
        });
        assert_eq!(
            missing_required(&schema, &json!({"a": "x"})),
            vec!["b".to_string()]
        );
        assert_eq!(
            missing_required(&schema, &json!({"a": "x", "b": null})),
            vec!["b".to_string()]
        );
        assert!(missing_required(&schema, &json!({"a": "x", "b": "y"})).is_empty());
        // "c" isn't required, so its absence doesn't matter.
        assert!(missing_required(&schema, &json!({"a": "x", "b": "y"})).is_empty());
    }

    #[test]
    fn missing_required_is_empty_when_schema_has_no_required_array() {
        assert!(missing_required(&empty_schema(), &json!({})).is_empty());
    }

    #[test]
    fn streaming_methods_match_admin_and_write_tools() {
        assert!(is_streaming_method("knowledge_rebuild_from_wal"));
        assert!(is_streaming_method("knowledge_canonicalize_relations"));
        assert!(is_streaming_method("knowledge_backfill_relation_types"));
        assert!(is_streaming_method("knowledge_reprocess_relation_types"));
        assert!(is_streaming_method("knowledge_reprocess_entity_types"));
        assert!(!is_streaming_method("knowledge_status"));
    }

    #[test]
    fn backfill_relation_types_description_warns_and_points_to_replacement() {
        let r = registry();
        let tool = r
            .iter()
            .find(|t| t.name == "knowledge_backfill_relation_types")
            .expect("knowledge_backfill_relation_types must remain registered");
        let desc = tool.description;
        assert!(
            desc.contains("DEPRECATED"),
            "description must mark the tool deprecated"
        );
        assert!(
            desc.contains("does not classify against the ontology"),
            "description must plainly state it does not classify against the ontology"
        );
        assert!(
            desc.contains("pollutes the relation_type space") && desc.contains("re-nulling"),
            "description must warn about pollution and reversibility only via re-nulling"
        );
        assert!(
            desc.contains("knowledge_reprocess_relation_types") && desc.contains("untyped"),
            "description must point callers to knowledge_reprocess_relation_types {{scope: \"untyped\"}}"
        );
    }
}
