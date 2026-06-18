use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use lbug::{LogicalType, Value};

use crate::{
    error::Error,
    types::{EntityRow, EpisodicRow, MentionsEdge, PassageResult, RelatesToEdge},
};

/// Map from entity UUID to (episode_uuids, source_descriptions), positionally aligned.
type EpisodeInfoMap = HashMap<String, (Vec<String>, Vec<String>)>;

pub struct Db {
    inner: lbug::Database,
}

pub struct Conn<'db> {
    inner: lbug::Connection<'db>,
    /// Recorded mutations as `(cypher_template, json_params)` pairs, in execution order.
    /// DDL / non-parameterized writes via `raw_query`/`cypher_query` record
    /// `(sql, Value::Null)`; value-bearing writes via `exec_params` record
    /// `(template, params)`. Callers drain this after a write and pass the pairs to a
    /// WAL-flush helper. Order-preserving so bound-param and raw paths interleave
    /// correctly. See `wal_exec.rs` for the drain-and-flush pattern (ADR-001).
    executed_mutations: RefCell<Vec<(String, serde_json::Value)>>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self, Error> {
        let inner = lbug::Database::new(path, lbug::SystemConfig::default())?;
        // Both INSTALL and LOAD EXTENSION are write transactions in lbug,
        // and both must run before any vector / FTS use. Extensions persist at
        // the Database level (not per-Connection), so we set them up once here
        // — running them in connect() races concurrent callers.
        let setup_conn = lbug::Connection::new(&inner)?;
        let _ = setup_conn.query("INSTALL vector")?;
        let _ = setup_conn.query("LOAD EXTENSION vector")?;
        let _ = setup_conn.query("INSTALL fts")?;
        let _ = setup_conn.query("LOAD EXTENSION fts")?;
        drop(setup_conn);
        Ok(Self { inner })
    }

    /// If `db_path` is absent but `wal_dir` contains `.jsonl` files, creates a fresh DB and
    /// replays the WAL to rebuild it (R-06). Otherwise behaves like `Db::open`.
    pub fn open_or_rebuild(
        db_path: &str,
        wal_dir: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let db_exists = Path::new(db_path).exists();
        let wal_dir_path = Path::new(wal_dir);

        let has_wal = wal_dir_path.exists()
            && wal_dir_path
                .read_dir()
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                })
                .unwrap_or(false);

        let db = Self::open(db_path)?;

        if !db_exists && has_wal {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            crate::replay::WalReplayer::new(wal_dir).replay(&conn)?;
        }

        Ok(db)
    }

    /// Opens a fresh connection against the already-set-up database.
    /// Extension setup happens once in `Db::open` because `INSTALL` and
    /// `LOAD EXTENSION` are both write transactions in lbug — running them
    /// per-connection serializes every connect() and races concurrent callers.
    pub fn connect(&self) -> Result<Conn<'_>, Error> {
        let conn = lbug::Connection::new(&self.inner)?;
        Ok(Conn {
            inner: conn,
            executed_mutations: RefCell::new(Vec::new()),
        })
    }
}

impl<'db> Conn<'db> {
    /// Runs a raw Cypher statement returning no rows; used for DDL (schema/index) and
    /// non-parameterized statements. Records `(sql, Null)` for WAL flushing by callers.
    ///
    /// Value-bearing writes should use [`Conn::exec_params`] instead, which binds typed
    /// parameters (no string interpolation, no escaping) and records the parameterized
    /// form into the WAL.
    pub(crate) fn raw_query(&self, sql: &str) -> Result<(), Error> {
        let _ = self.inner.query(sql)?;
        self.executed_mutations
            .borrow_mut()
            .push((sql.to_string(), serde_json::Value::Null));
        Ok(())
    }

    /// Executes a parameterized Cypher statement via lbug prepared-statement binding,
    /// then records `(template, params)` for WAL flushing.
    ///
    /// This is the bound-parameter write path: values are bound as typed lbug `Value`s
    /// (never interpolated into the query text), so no escaping is required and lbug
    /// coerces each bound value to its destination column type (e.g. an RFC-3339 string
    /// into a `TIMESTAMP` column, a numeric list into a `FLOAT[N]` column).
    ///
    /// `cypher` must use `$name` placeholders matching keys in the `params` JSON object.
    pub(crate) fn exec_params(&self, cypher: &str, params: serde_json::Value) -> Result<(), Error> {
        let mut prepared = self.inner.prepare(cypher)?;
        self.execute_prepared(&mut prepared, &params)?;
        self.executed_mutations
            .borrow_mut()
            .push((cypher.to_string(), params));
        Ok(())
    }

    /// Prepares a parameterized Cypher statement for repeated execution. Used by the WAL
    /// replay path to prepare a template once and execute many rows against it (the
    /// throughput win over re-planning per row), via [`Conn::execute_prepared`].
    pub(crate) fn prepare(&self, cypher: &str) -> Result<lbug::PreparedStatement, Error> {
        Ok(self.inner.prepare(cypher)?)
    }

    /// Binds `params` and executes an already-prepared statement. Does **not** record to the
    /// WAL — used by WAL replay (which is rebuilding *from* the WAL and must not re-log) and
    /// internally by [`Conn::exec_params`] (which records separately on success).
    pub(crate) fn execute_prepared(
        &self,
        prepared: &mut lbug::PreparedStatement,
        params: &serde_json::Value,
    ) -> Result<(), Error> {
        // Keep the keys alive in `keys` so we can hand lbug `&str` borrows alongside the
        // owned Values it consumes.
        let (keys, vals): (Vec<String>, Vec<Value>) =
            json_params_to_values(params).into_iter().unzip();
        let bound: Vec<(&str, Value)> = keys.iter().map(|k| k.as_str()).zip(vals).collect();
        self.inner.execute(prepared, bound)?;
        Ok(())
    }

    /// Runs a parameterized read query via prepared-statement binding and materializes the
    /// result rows. Used by read paths so query values are bound (no string interpolation /
    /// escaping). Does not record to the WAL (reads are not mutations).
    ///
    /// Rows are collected into a `Vec` before returning so the result does not borrow the
    /// transient `PreparedStatement`.
    pub(crate) fn query_params(
        &self,
        cypher: &str,
        params: serde_json::Value,
    ) -> Result<Vec<Vec<Value>>, Error> {
        let mut prepared = self.inner.prepare(cypher)?;
        let (keys, vals): (Vec<String>, Vec<Value>) =
            json_params_to_values(&params).into_iter().unzip();
        let bound: Vec<(&str, Value)> = keys.iter().map(|k| k.as_str()).zip(vals).collect();
        let result = self.inner.execute(&mut prepared, bound)?;
        Ok(result.collect())
    }

    /// Public pass-through for raw Cypher statements with no result rows.
    pub fn run_cypher(&self, sql: &str) -> Result<(), Error> {
        self.raw_query(sql)
    }

    /// Runs a raw Cypher SELECT and returns all rows as lbug Values.
    pub fn query_cypher_raw(
        &self,
        sql: &str,
    ) -> Result<impl Iterator<Item = Vec<lbug::Value>> + '_, Error> {
        let result = self.inner.query(sql)?;
        Ok(result)
    }

    /// Runs a raw Cypher SELECT and returns rows as string columns (T012 pass-through).
    /// Records `(sql, Null)` on success so `handle_query_cypher` can WAL-log mutation
    /// queries issued via this escape hatch.
    pub fn cypher_query(&self, sql: &str) -> Result<Vec<Vec<String>>, Error> {
        let result = self.inner.query(sql)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(row.iter().map(value_as_string).collect());
        }
        self.executed_mutations
            .borrow_mut()
            .push((sql.to_string(), serde_json::Value::Null));
        Ok(rows)
    }

    /// Drains and returns all `(cypher_template, params)` mutations recorded since the
    /// last drain (or since the connection was opened). Pass the result to
    /// `wal_exec::wal_flush_chunk` or `wal_exec::wal_flush_ungrouped` to append them to
    /// the application WAL. Non-mutations are silently filtered inside
    /// `WalWriter::log_mutation`.
    pub fn drain_mutations(&self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut *self.executed_mutations.borrow_mut())
    }

    /// Creates the Entity and Episodic node tables. Call once after connecting.
    pub fn init_schema(&self, embedding_dim: usize) -> Result<(), Error> {
        crate::schema::init(self, embedding_dim)?;
        crate::schema::migrate(self);
        Ok(())
    }

    /// Creates HNSW vector indexes and FTS indexes; idempotent.
    pub fn build_indices_and_constraints(&self) -> Result<(), Error> {
        self.create_vector_indexes()?;
        crate::schema::create_fts_indexes(self)
    }

    // ── Entity/Episodic insert ─────────────────────────────────────────────────

    pub fn insert_entity(&self, row: &EntityRow) -> Result<(), Error> {
        // Enforce Entity-first label-order invariant (AD-8)
        let labels = enforce_entity_first(&row.labels);
        self.exec_params(
            "CREATE (:Entity {uuid: $uuid, name: $name, group_id: $group_id, \
             labels: $labels, created_at: $created_at, name_embedding: $name_embedding, \
             summary: $summary, attributes: $attributes})",
            serde_json::json!({
                "uuid": row.uuid,
                "name": row.name,
                "group_id": row.group_id,
                "labels": labels,
                "created_at": row.created_at,
                "name_embedding": row.name_embedding,
                "summary": row.summary,
                "attributes": row.attributes,
            }),
        )
    }

    pub fn insert_episodic(&self, row: &EpisodicRow) -> Result<(), Error> {
        self.exec_params(
            "CREATE (:Episodic {uuid: $uuid, name: $name, group_id: $group_id, \
             created_at: $created_at, source: $source, source_description: $source_description, \
             content: $content, content_embedding: $content_embedding, valid_at: $valid_at, \
             entity_edges: $entity_edges})",
            serde_json::json!({
                "uuid": row.uuid,
                "name": row.name,
                "group_id": row.group_id,
                "created_at": row.created_at,
                "source": row.source,
                "source_description": row.source_description,
                "content": row.content,
                "content_embedding": row.content_embedding,
                "valid_at": row.valid_at,
                "entity_edges": row.entity_edges,
            }),
        )
    }

    // ── Edge insert ───────────────────────────────────────────────────────────

    /// Inserts a RELATES_TO rel edge and the corresponding RelatesToNode_ shadow node.
    pub fn insert_relates_to_edge(&self, edge: &RelatesToEdge) -> Result<(), Error> {
        // Shadow node for vector search. Nullable fields (valid_at/invalid_at/relation_type)
        // bind as JSON null when absent; lbug accepts null into the nullable columns.
        self.exec_params(
            "CREATE (:RelatesToNode_ {uuid: $uuid, name: $name, group_id: $group_id, \
             created_at: $created_at, fact: $fact, fact_embedding: $fact_embedding, \
             valid_at: $valid_at, invalid_at: $invalid_at, attributes: $attributes, \
             relation_type: $relation_type})",
            serde_json::json!({
                "uuid": edge.uuid,
                "name": edge.name,
                "group_id": edge.group_id,
                "created_at": edge.created_at,
                "fact": edge.fact,
                "fact_embedding": edge.fact_embedding,
                "valid_at": edge.valid_at,
                "invalid_at": edge.invalid_at,
                "attributes": edge.attributes,
                "relation_type": edge.relation_type,
            }),
        )?;

        // Direct Entity→Entity rel is non-fatal: Python-schema DBs have no Entity→Entity
        // FROM-TO pair in RELATES_TO, so this insert will fail there. The two-hop links
        // below are sufficient for all reads; the direct rel is kept for schema compatibility
        // with Rust-initialized DBs only. (exec_params records to the WAL only on success,
        // so a failed insert here is not WAL-logged — matching the prior raw_query behavior.)
        if let Err(e) = self.exec_params(
            "MATCH (src:Entity {uuid: $src}), (dst:Entity {uuid: $dst}) \
             CREATE (src)-[:RELATES_TO {uuid: $uuid, name: $name, group_id: $group_id, \
             fact: $fact, valid_at: $valid_at, invalid_at: $invalid_at, \
             attributes: $attributes}]->(dst)",
            serde_json::json!({
                "src": edge.source_node_uuid,
                "dst": edge.target_node_uuid,
                "uuid": edge.uuid,
                "name": edge.name,
                "group_id": edge.group_id,
                "fact": edge.fact,
                "valid_at": edge.valid_at,
                "invalid_at": edge.invalid_at,
                "attributes": edge.attributes,
            }),
        ) {
            eprintln!(
                "liminis-graph: direct RELATES_TO rel insert failed (non-fatal, Python-schema DB?): {e}"
            );
        }

        // Create both two-hop links in a single statement so either both exist or neither does.
        // Reads use Entity→RelatesToNode_→Entity; the hops carry no meaningful properties —
        // all edge data lives on the RelatesToNode_ shadow node.
        self.exec_params(
            "MATCH (src:Entity {uuid: $src}), \
                   (rn:RelatesToNode_ {uuid: $rn}), \
                   (dst:Entity {uuid: $dst}) \
             CREATE (src)-[:RELATES_TO]->(rn), (rn)-[:RELATES_TO]->(dst)",
            serde_json::json!({
                "src": edge.source_node_uuid,
                "rn": edge.uuid,
                "dst": edge.target_node_uuid,
            }),
        )
    }

    pub fn insert_mentions_edge(&self, e: &MentionsEdge) -> Result<(), Error> {
        self.exec_params(
            "MATCH (ep:Episodic {uuid: $ep}), (en:Entity {uuid: $en}) \
             CREATE (ep)-[:MENTIONS {group_id: $group_id}]->(en)",
            serde_json::json!({
                "ep": e.episodic_uuid,
                "en": e.entity_uuid,
                "group_id": e.group_id,
            }),
        )
    }

    // ── HNSW / FTS indexes ────────────────────────────────────────────────────

    /// Creates HNSW vector indexes on Entity and Episodic.
    pub fn create_vector_indexes(&self) -> Result<(), Error> {
        // Suppress errors for "already exists" — idempotent
        let _ = self.raw_query(
            "CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', \
             'name_embedding', metric := 'cosine')",
        );
        let _ = self.raw_query(
            "CALL CREATE_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', \
             'content_embedding', metric := 'cosine')",
        );
        let _ = self.raw_query(
            "CALL CREATE_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx', \
             'fact_embedding', metric := 'cosine')",
        );
        Ok(())
    }

    // ── Retrieval ─────────────────────────────────────────────────────────────

    /// Returns the last `last_n` episodic nodes for a given group, newest first.
    pub fn retrieve_episodes(
        &self,
        group_id: &str,
        last_n: usize,
    ) -> Result<Vec<EpisodicRow>, Error> {
        let result = self.query_params(
            "MATCH (ep:Episodic) WHERE ep.group_id = $gid \
             RETURN ep.uuid, ep.name, ep.group_id, ep.created_at, ep.source, \
             ep.source_description, ep.content, ep.valid_at, ep.entity_edges \
             ORDER BY ep.created_at DESC LIMIT $limit",
            serde_json::json!({ "gid": group_id, "limit": last_n as i64 }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EpisodicRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                created_at: value_as_timestamp_str(&row[3]),
                source: value_as_string(&row[4]),
                source_description: value_as_string(&row[5]),
                content: value_as_string(&row[6]),
                valid_at: value_as_timestamp_str(&row[7]),
                entity_edges: value_as_str_list(&row[8]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Deletes an Episodic node and all its connected edges.
    pub fn remove_episode(&self, episode_uuid: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (ep:Episodic {uuid: $uuid}) DETACH DELETE ep",
            serde_json::json!({ "uuid": episode_uuid }),
        )
    }

    /// Deletes all Episodic nodes whose source_description equals source_file or starts with
    /// source_file + ":". Returns the UUIDs of deleted episodes.
    ///
    /// When group_ids is Some, only episodes in those groups are considered.
    pub fn remove_episodes_by_source(
        &self,
        source_file: &str,
        group_ids: Option<&[&str]>,
    ) -> Result<Vec<String>, Error> {
        let group_clause = match group_ids {
            Some(ids) if !ids.is_empty() => " AND ep.group_id IN $gids",
            _ => "",
        };
        let prefix = format!("{}:", source_file);
        let match_sql = format!(
            "MATCH (ep:Episodic) WHERE (ep.source_description = $src \
             OR ep.source_description STARTS WITH $prefix){group_clause} RETURN ep.uuid"
        );
        let mut params = serde_json::json!({ "src": source_file, "prefix": prefix });
        if let Some(ids) = group_ids {
            if !ids.is_empty() {
                params["gids"] = serde_json::json!(ids);
            }
        }
        let uuids: Vec<String> = self
            .query_params(&match_sql, params)?
            .into_iter()
            .map(|row| value_as_string(&row[0]))
            .collect();
        if !uuids.is_empty() {
            self.exec_params(
                "MATCH (ep:Episodic) WHERE ep.uuid IN $uuids DETACH DELETE ep",
                serde_json::json!({ "uuids": uuids }),
            )?;
        }
        Ok(uuids)
    }

    /// Deletes all Episodic nodes whose name (chunk identifier) matches chunk_id.
    /// Returns the UUIDs of deleted episodes.
    ///
    /// Matches on ep.name (which always stores chunk_id) rather than source_description.
    /// Orphaned entities connected only to the deleted episodes are NOT removed — callers
    /// should be aware that entity nodes may become disconnected after this call.
    ///
    /// When group_ids is Some, only episodes in those groups are considered.
    pub fn remove_episodes_by_chunk_id(
        &self,
        chunk_id: &str,
        group_ids: Option<&[&str]>,
    ) -> Result<Vec<String>, Error> {
        let group_clause = match group_ids {
            Some(ids) if !ids.is_empty() => " AND ep.group_id IN $gids",
            _ => "",
        };
        let match_sql =
            format!("MATCH (ep:Episodic) WHERE ep.name = $name{group_clause} RETURN ep.uuid");
        let mut params = serde_json::json!({ "name": chunk_id });
        if let Some(ids) = group_ids {
            if !ids.is_empty() {
                params["gids"] = serde_json::json!(ids);
            }
        }
        let uuids: Vec<String> = self
            .query_params(&match_sql, params)?
            .into_iter()
            .map(|row| value_as_string(&row[0]))
            .collect();
        if !uuids.is_empty() {
            self.exec_params(
                "MATCH (ep:Episodic) WHERE ep.uuid IN $uuids DETACH DELETE ep",
                serde_json::json!({ "uuids": uuids }),
            )?;
        }
        Ok(uuids)
    }

    /// Returns all Entity nodes in the given group_ids.
    pub fn get_entities_by_group_ids(&self, group_ids: &[&str]) -> Result<Vec<EntityRow>, Error> {
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.group_id IN $gids \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes",
            serde_json::json!({ "gids": group_ids }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Returns all RELATES_TO edges in the given group_ids.
    pub fn get_edges_by_group_ids(&self, group_ids: &[&str]) -> Result<Vec<RelatesToEdge>, Error> {
        self.collect_relates_to_edges(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.group_id IN $gids \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "gids": group_ids }),
        )
    }

    /// Returns RELATES_TO edges for the given UUIDs.
    pub fn get_edges_by_uuids(&self, uuids: &[&str]) -> Result<Vec<RelatesToEdge>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        self.collect_relates_to_edges(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.uuid IN $uuids \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "uuids": uuids }),
        )
    }

    fn collect_relates_to_edges(
        &self,
        cypher: &str,
        params: serde_json::Value,
    ) -> Result<Vec<RelatesToEdge>, Error> {
        let result = self.query_params(cypher, params)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(RelatesToEdge {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                source_node_uuid: value_as_string(&row[2]),
                target_node_uuid: value_as_string(&row[3]),
                group_id: value_as_string(&row[4]),
                fact: value_as_string(&row[5]),
                valid_at: value_as_optional_timestamp_str(&row[6]),
                invalid_at: value_as_optional_timestamp_str(&row[7]),
                attributes: value_as_string(&row[8]),
                relation_type: value_as_optional_string(&row[9]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    // ── Search helpers ────────────────────────────────────────────────────────

    /// BM25 full-text search on Entity nodes; returns (uuid, score) pairs.
    pub fn fts_search_entities(
        &self,
        query: &str,
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        self.collect_uuid_score_pairs(
            "CALL QUERY_FTS_INDEX('Entity', 'node_name_and_summary', $q) \
             WITH node, score WHERE node.group_id IN $gids \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT $limit",
            serde_json::json!({ "q": query, "gids": group_ids, "limit": limit as i64 }),
        )
    }

    /// BM25 full-text search on RelatesToNode_ (facts); returns (uuid, score) pairs.
    pub fn fts_search_edges(
        &self,
        query: &str,
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        self.collect_uuid_score_pairs(
            "CALL QUERY_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', $q) \
             WITH node, score WHERE node.group_id IN $gids \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT $limit",
            serde_json::json!({ "q": query, "gids": group_ids, "limit": limit as i64 }),
        )
    }

    /// HNSW vector search on Entity nodes; returns (uuid, distance) pairs (lower = closer).
    pub fn vector_search_entities(
        &self,
        embedding: &[f32],
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        self.collect_uuid_score_pairs(
            "CALL QUERY_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', $emb, $limit) \
             WITH node, distance WHERE node.group_id IN $gids \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT $limit",
            serde_json::json!({ "emb": embedding, "gids": group_ids, "limit": limit as i64 }),
        )
    }

    /// HNSW vector search on RelatesToNode_ (facts); returns (uuid, distance) pairs.
    pub fn vector_search_edges(
        &self,
        embedding: &[f32],
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        self.collect_uuid_score_pairs(
            "CALL QUERY_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx', \
             $emb, $limit) \
             WITH node, distance WHERE node.group_id IN $gids \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT $limit",
            serde_json::json!({ "emb": embedding, "gids": group_ids, "limit": limit as i64 }),
        )
    }

    /// HNSW vector search on Episodic nodes; returns PassageResult rows with score = raw distance.
    /// Caller must convert distance → similarity: `score = 1.0 - distance`.
    /// Optional `group_ids` filter is pushed into the Cypher WHERE clause after the HNSW scan.
    pub fn vector_search_episodic(
        &self,
        embedding: &[f32],
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<PassageResult>, Error> {
        let gid_filter = match group_ids {
            Some(gids) if !gids.is_empty() => "WHERE node.group_id IN $gids",
            _ => "",
        };
        let cypher = format!(
            "CALL QUERY_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', $emb, $limit) \
             WITH node, distance {gid_filter} \
             RETURN node.uuid, node.name, node.content, node.source_description, \
             node.group_id, node.created_at, node.valid_at, distance \
             ORDER BY distance ASC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "emb": embedding, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            if !gids.is_empty() {
                params["gids"] = serde_json::json!(gids);
            }
        }
        let result = self.query_params(&cypher, params)?;
        let mut rows = Vec::new();
        for row in result {
            let valid_at = match value_as_optional_timestamp_str(&row[6]) {
                Some(s) if s.is_empty() => None,
                other => other,
            };
            rows.push(PassageResult {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                content: value_as_string(&row[2]),
                source_description: value_as_string(&row[3]),
                group_id: value_as_string(&row[4]),
                created_at: value_as_timestamp_str(&row[5]),
                valid_at,
                score: value_as_f64(&row[7]),
            });
        }
        Ok(rows)
    }

    /// Lists Entity nodes with optional group filter, ordered by uuid DESC.
    pub fn list_entities(
        &self,
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<EntityRow>, Error> {
        let (cypher, params) = match group_ids {
            Some(gids) if !gids.is_empty() => (
                "MATCH (e:Entity) WHERE e.group_id IN $gids \
                 RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes ORDER BY e.uuid DESC LIMIT $limit",
                serde_json::json!({ "gids": gids, "limit": limit as i64 }),
            ),
            _ => (
                "MATCH (e:Entity) \
                 RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes ORDER BY e.uuid DESC LIMIT $limit",
                serde_json::json!({ "limit": limit as i64 }),
            ),
        };
        let result = self.query_params(cypher, params)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Lists RELATES_TO edges with optional group filter, ordered by uuid DESC.
    pub fn list_relationships(
        &self,
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<RelatesToEdge>, Error> {
        let (cypher, params) = match group_ids {
            Some(gids) if !gids.is_empty() => (
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 WHERE rn.group_id IN $gids \
                 RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                 rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type ORDER BY rn.uuid DESC LIMIT $limit",
                serde_json::json!({ "gids": gids, "limit": limit as i64 }),
            ),
            _ => (
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                 rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type ORDER BY rn.uuid DESC LIMIT $limit",
                serde_json::json!({ "limit": limit as i64 }),
            ),
        };
        self.collect_relates_to_edges(cypher, params)
    }

    /// Returns 1-hop neighbors via two directed queries (outgoing + incoming), merged in Rust.
    /// Returns `(edges, unique_neighbor_uuids)` truncated to `num_results` edges.
    pub fn get_entity_neighbors(
        &self,
        entity_uuid: &str,
        group_ids: Option<&[&str]>,
        num_results: usize,
    ) -> Result<(Vec<RelatesToEdge>, Vec<String>), Error> {
        let gid_filter = match group_ids {
            Some(gids) if !gids.is_empty() => "WHERE rn.group_id IN $gids",
            _ => "",
        };
        let mk_params = || {
            let mut p = serde_json::json!({ "uuid": entity_uuid, "limit": num_results as i64 });
            if let Some(gids) = group_ids {
                if !gids.is_empty() {
                    p["gids"] = serde_json::json!(gids);
                }
            }
            p
        };

        let out_sql = format!(
            "MATCH (c:Entity {{uuid: $uuid}})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(n:Entity) \
             {gid_filter} \
             RETURN rn.uuid, rn.name, c.uuid, n.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type ORDER BY rn.uuid DESC LIMIT $limit"
        );
        let in_sql = format!(
            "MATCH (n:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(c:Entity {{uuid: $uuid}}) \
             {gid_filter} \
             RETURN rn.uuid, rn.name, n.uuid, c.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type ORDER BY rn.uuid DESC LIMIT $limit"
        );

        let mut edges = self.collect_relates_to_edges(&out_sql, mk_params())?;
        edges.extend(self.collect_relates_to_edges(&in_sql, mk_params())?);
        edges.truncate(num_results);

        let mut seen = std::collections::HashSet::new();
        let mut neighbor_uuids: Vec<String> = Vec::new();
        for edge in &edges {
            let neighbor = if edge.source_node_uuid == entity_uuid {
                edge.target_node_uuid.clone()
            } else {
                edge.source_node_uuid.clone()
            };
            if seen.insert(neighbor.clone()) {
                neighbor_uuids.push(neighbor);
            }
        }

        Ok((edges, neighbor_uuids))
    }

    /// Returns Entity nodes reachable via Episodic nodes whose source_description CONTAINS `source`.
    ///
    /// Uses Cypher `CONTAINS` predicate (substring semantics, FR-017). If lbug's dialect does not
    /// support `CONTAINS`, this will return an error and the caller should fall back to Rust-side
    /// filtering.
    pub fn get_entities_by_source(
        &self,
        source: &str,
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<EntityRow>, Error> {
        let (cypher, params): (&str, serde_json::Value) = match group_ids {
            Some(gids) if !gids.is_empty() => (
                "MATCH (ep:Episodic)-[:MENTIONS]->(e:Entity) \
                 WHERE ep.source_description CONTAINS $src AND e.group_id IN $gids \
                 RETURN DISTINCT e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes LIMIT $limit",
                serde_json::json!({ "src": source, "gids": gids, "limit": limit as i64 }),
            ),
            _ => (
                "MATCH (ep:Episodic)-[:MENTIONS]->(e:Entity) \
                 WHERE ep.source_description CONTAINS $src \
                 RETURN DISTINCT e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes LIMIT $limit",
                serde_json::json!({ "src": source, "limit": limit as i64 }),
            ),
        };
        let result = self.query_params(cypher, params)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    fn collect_uuid_score_pairs(
        &self,
        cypher: &str,
        params: serde_json::Value,
    ) -> Result<Vec<(String, f64)>, Error> {
        let result = self.query_params(cypher, params)?;
        let mut pairs = Vec::new();
        for row in result {
            let uuid = value_as_string(&row[0]);
            let score = value_as_f64(&row[1]);
            pairs.push((uuid, score));
        }
        Ok(pairs)
    }

    /// Brute-force cosine similarity to find the best-matching Entity in a group (AD-4).
    pub fn brute_force_similar_entity(
        &self,
        name_embedding: &[f32],
        group_id: &str,
        threshold: f32,
    ) -> Result<Option<EntityRow>, Error> {
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.group_id = $gid \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.name_embedding, e.summary, e.attributes",
            serde_json::json!({ "gid": group_id }),
        )?;
        let mut best: Option<(f32, EntityRow)> = None;

        for row in result {
            let stored_embedding = value_as_float_array(&row[5]);
            if stored_embedding.is_empty() {
                continue;
            }
            let sim = cosine_similarity(name_embedding, &stored_embedding);
            if sim >= threshold {
                let candidate_uuid = value_as_string(&row[0]);
                let is_better = best
                    .as_ref()
                    .is_none_or(|(s, r)| sim > *s || (sim == *s && candidate_uuid < r.uuid));
                if is_better {
                    best = Some((
                        sim,
                        EntityRow {
                            uuid: candidate_uuid,
                            name: value_as_string(&row[1]),
                            group_id: value_as_string(&row[2]),
                            labels: value_as_str_list(&row[3]),
                            created_at: value_as_timestamp_str(&row[4]),
                            name_embedding: stored_embedding,
                            summary: value_as_string(&row[6]),
                            attributes: value_as_string(&row[7]),
                            episode_uuids: vec![],
                            source_descriptions: vec![],
                        },
                    ));
                }
            }
        }
        Ok(best.map(|(_, row)| row))
    }

    /// Returns the number of Entity nodes in the given group. Returns 0 when the group is empty.
    pub fn entity_count_in_group(&self, group_id: &str) -> Result<usize, Error> {
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE e.group_id = $gid RETURN count(e)",
            serde_json::json!({ "gid": group_id }),
        )?;
        if let Some(row) = rows.into_iter().next() {
            Ok(value_as_usize(&row[0]))
        } else {
            Ok(0)
        }
    }

    /// Fetches (uuid, name_embedding) pairs for a slice of UUIDs.
    /// Excludes entities whose stored embedding is empty.
    pub fn get_entity_embeddings_by_uuids(
        &self,
        uuids: &[String],
    ) -> Result<Vec<(String, Vec<f32>)>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.uuid IN $uuids RETURN e.uuid, e.name_embedding",
            serde_json::json!({ "uuids": uuids }),
        )?;
        let mut pairs = Vec::new();
        for row in result {
            let emb = value_as_float_array(&row[1]);
            if !emb.is_empty() {
                pairs.push((value_as_string(&row[0]), emb));
            }
        }
        Ok(pairs)
    }

    /// Hybrid HNSW + BM25 dedup: retrieves CANDIDATE_K candidates per path, fuses with RRF,
    /// cosine-rechecks the full fused set against `threshold`, and returns the best match.
    ///
    /// Note: the `ef` search parameter is not configurable in lbug 0.17.0; the lbug default is used.
    pub fn hybrid_dedup_similar_entity(
        &self,
        name_embedding: &[f32],
        entity_name: &str,
        group_id: &str,
        threshold: f32,
    ) -> Result<Option<EntityRow>, Error> {
        const CANDIDATE_K: usize = 200;

        let vector_candidates =
            self.vector_search_entities(name_embedding, &[group_id], CANDIDATE_K)?;
        let bm25_candidates = self.fts_search_entities(entity_name, &[group_id], CANDIDATE_K)?;
        let fused_uuids = crate::search::rrf_fuse(&bm25_candidates, &vector_candidates);

        let candidate_embeddings = self.get_entity_embeddings_by_uuids(&fused_uuids)?;

        let mut best: Option<(f32, String)> = None;
        for (uuid, emb) in candidate_embeddings {
            let sim = cosine_similarity(name_embedding, &emb);
            if sim >= threshold {
                let is_better = best
                    .as_ref()
                    .is_none_or(|(s, best_uuid)| sim > *s || (sim == *s && &uuid < best_uuid));
                if is_better {
                    best = Some((sim, uuid));
                }
            }
        }

        if let Some((_, uuid)) = best {
            self.get_entity_by_uuid(&uuid)
        } else {
            Ok(None)
        }
    }

    /// Returns an EntityRow by exact name match. Returns the first match if multiple exist.
    pub fn get_entity_by_name(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Option<EntityRow>, Error> {
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE e.name = $name AND e.group_id = $gid \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes LIMIT 1",
            serde_json::json!({ "name": name, "gid": group_id }),
        )?;
        if let Some(row) = rows.into_iter().next() {
            Ok(Some(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            }))
        } else {
            Ok(None)
        }
    }

    /// Returns a full EntityRow by UUID.
    pub fn get_entity_by_uuid(&self, uuid: &str) -> Result<Option<EntityRow>, Error> {
        let rows = self.query_params(
            "MATCH (e:Entity {uuid: $uuid}) \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.name_embedding, e.summary, e.attributes",
            serde_json::json!({ "uuid": uuid }),
        )?;
        if let Some(row) = rows.into_iter().next() {
            Ok(Some(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                name_embedding: value_as_float_array(&row[5]),
                summary: value_as_string(&row[6]),
                attributes: value_as_string(&row[7]),
                episode_uuids: vec![],
                source_descriptions: vec![],
            }))
        } else {
            Ok(None)
        }
    }

    /// Fetches full EntityRows for a slice of UUIDs (for search result expansion).
    pub fn get_entities_by_uuids(&self, uuids: &[String]) -> Result<Vec<EntityRow>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.uuid IN $uuids \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes",
            serde_json::json!({ "uuids": uuids }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Batch-fetches episode info for a set of entity UUIDs via the MENTIONS relationship.
    ///
    /// Returns a map from entity UUID → (episode_uuids, source_descriptions), positionally
    /// aligned. Short-circuits to an empty map when `entity_uuids` is empty.
    /// Optional `group_ids` filter restricts which episodes are returned.
    pub fn get_episode_info_for_entities(
        &self,
        entity_uuids: &[&str],
        group_ids: Option<&[&str]>,
    ) -> Result<EpisodeInfoMap, Error> {
        if entity_uuids.is_empty() {
            return Ok(HashMap::new());
        }
        let gid_clause = match group_ids {
            Some(gids) if !gids.is_empty() => " AND ep.group_id IN $gids",
            _ => "",
        };
        let sql = format!(
            "MATCH (ep:Episodic)-[:MENTIONS]->(n:Entity) \
             WHERE n.uuid IN $uuids{gid_clause} \
             RETURN DISTINCT n.uuid, ep.uuid, ep.source_description"
        );
        let mut params = serde_json::json!({ "uuids": entity_uuids });
        if let Some(gids) = group_ids {
            if !gids.is_empty() {
                params["gids"] = serde_json::json!(gids);
            }
        }
        let result = self.query_params(&sql, params)?;
        let mut map: EpisodeInfoMap = HashMap::new();
        for row in result {
            let entity_uuid = value_as_string(&row[0]);
            let ep_uuid = value_as_string(&row[1]);
            let src_desc = value_as_string(&row[2]);
            let entry = map.entry(entity_uuid).or_default();
            entry.0.push(ep_uuid);
            entry.1.push(src_desc);
        }
        Ok(map)
    }

    /// Fetches full RelatesToEdge rows for a slice of UUIDs from RelatesToNode_.
    pub fn get_relates_to_by_uuids(&self, uuids: &[String]) -> Result<Vec<RelatesToEdge>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        // Resolve src/dst via the two-hop links (Entity→RelatesToNode_→Entity).
        let result = self.query_params(
            "MATCH (rn:RelatesToNode_) WHERE rn.uuid IN $uuids \
             OPTIONAL MATCH (src:Entity)-[:RELATES_TO]->(rn) \
             OPTIONAL MATCH (rn)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, coalesce(src.uuid, ''), coalesce(dst.uuid, ''), \
             rn.group_id, rn.fact, rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "uuids": uuids }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(RelatesToEdge {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                source_node_uuid: value_as_string(&row[2]),
                target_node_uuid: value_as_string(&row[3]),
                group_id: value_as_string(&row[4]),
                fact: value_as_string(&row[5]),
                valid_at: value_as_optional_timestamp_str(&row[6]),
                invalid_at: value_as_optional_timestamp_str(&row[7]),
                attributes: value_as_string(&row[8]),
                relation_type: value_as_optional_string(&row[9]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Returns the count of nodes with the given label.
    ///
    /// Returns `Err` if `label` contains characters that are not alphanumeric or `_`
    /// (labels cannot be parameterized in Cypher, so we validate before interpolation).
    pub fn count_nodes(&self, label: &str) -> Result<u64, Error> {
        if !label.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(Error::QueryFailed(format!(
                "invalid label identifier: {label}"
            )));
        }
        let sql = format!("MATCH (n:{label}) RETURN count(*)");
        let result = self.inner.query(&sql)?;
        for row in result {
            match &row[0] {
                lbug::Value::Int64(n) => return Ok(*n as u64),
                lbug::Value::UInt64(n) => return Ok(*n),
                lbug::Value::Int32(n) => return Ok(*n as u64),
                _ => {}
            }
        }
        Ok(0)
    }

    /// Returns the count of RELATES_TO relationship edges.
    ///
    /// Uses the RelatesToNode_ shadow node count (1:1 with RELATES_TO rels, always maintained
    /// by insert_relates_to_edge) to avoid relying on an unverified rel-table Cypher pattern.
    pub fn count_relates_to_edges(&self) -> Result<u64, Error> {
        self.count_nodes("RelatesToNode_")
    }

    /// Returns the `created_at` of the most-recently created Episodic node, or `None` if there
    /// are no episodes yet.
    pub fn get_latest_episode_time(&self) -> Result<Option<String>, Error> {
        let result = self.inner.query(
            "MATCH (ep:Episodic) RETURN ep.created_at ORDER BY ep.created_at DESC LIMIT 1",
        )?;
        Ok(result
            .into_iter()
            .next()
            .and_then(|row| value_as_optional_timestamp_str(&row[0])))
    }

    /// Returns the uuid of the most-recently created Episodic node across all groups, or `None`
    /// if there are no episodes yet. Used by episode-cursor derivation during WAL recovery.
    pub fn get_latest_episode_uuid(&self) -> Result<Option<String>, Error> {
        let result = self
            .inner
            .query("MATCH (ep:Episodic) RETURN ep.uuid ORDER BY ep.created_at DESC LIMIT 1")?;
        Ok(result
            .into_iter()
            .next()
            .and_then(|row| value_as_optional_string(&row[0])))
    }

    /// Returns the earliest episode creation time as an ISO 8601 string, or None if empty.
    pub fn get_earliest_episode_time(&self) -> Result<Option<String>, Error> {
        let mut result = self
            .inner
            .query("MATCH (ep:Episodic) RETURN ep.created_at ORDER BY ep.created_at ASC LIMIT 1")
            .map_err(|e| Error::QueryFailed(format!("get_earliest_episode_time failed: {e}")))?;
        if let Some(row) = result.next() {
            match &row[0] {
                lbug::Value::Null(_) => return Ok(None),
                lbug::Value::Timestamp(dt) => {
                    return Ok(Some(format_datetime_iso8601(*dt)));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Cheap health probe — runs `RETURN 1` to verify the DB is queryable.
    pub fn probe(&self) -> Result<(), Error> {
        self.inner
            .query("RETURN 1")
            .map_err(|e| Error::QueryFailed(format!("health probe failed: {e}")))?;
        Ok(())
    }

    // ── Corrections support ───────────────────────────────────────────────────

    /// Returns edges for an entity including fact_embedding from the RelatesToNode_ shadow node.
    /// Used by same_as corrections to copy edges from alias to canonical with intact embeddings.
    pub fn get_full_edges_for_entity(
        &self,
        entity_uuid: &str,
    ) -> Result<Vec<RelatesToEdge>, Error> {
        // Outgoing edges (entity is source)
        let mut edges = self.collect_full_relates_to_edges(
            "MATCH (src:Entity {uuid: $uuid})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.fact_embedding, rn.created_at, rn.relation_type",
            serde_json::json!({ "uuid": entity_uuid }),
        )?;
        // Incoming edges (entity is target)
        edges.extend(self.collect_full_relates_to_edges(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity {uuid: $uuid}) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.fact_embedding, rn.created_at, rn.relation_type",
            serde_json::json!({ "uuid": entity_uuid }),
        )?);
        Ok(edges)
    }

    fn collect_full_relates_to_edges(
        &self,
        cypher: &str,
        params: serde_json::Value,
    ) -> Result<Vec<RelatesToEdge>, Error> {
        let result = self.query_params(cypher, params)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(RelatesToEdge {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                source_node_uuid: value_as_string(&row[2]),
                target_node_uuid: value_as_string(&row[3]),
                group_id: value_as_string(&row[4]),
                fact: value_as_string(&row[5]),
                valid_at: value_as_optional_timestamp_str(&row[6]),
                invalid_at: value_as_optional_timestamp_str(&row[7]),
                attributes: value_as_string(&row[8]),
                fact_embedding: value_as_float_array(&row[9]),
                created_at: value_as_timestamp_str(&row[10]),
                relation_type: value_as_optional_string(&row[11]),
                episode_uuids: vec![],
                source_descriptions: vec![],
            });
        }
        Ok(rows)
    }

    /// Checks whether a directed RELATES_TO edge with the given `name` already exists from
    /// `source_uuid` to `target_uuid`. The name filter prevents over-deduplication when the
    /// canonical entity has semantically different relationships to the same target.
    pub fn has_directed_edge(
        &self,
        source_uuid: &str,
        target_uuid: &str,
        name: &str,
    ) -> Result<bool, Error> {
        let rows = self.query_params(
            "MATCH (src:Entity {uuid: $src})-[:RELATES_TO]->(rn:RelatesToNode_ {name: $name})-[:RELATES_TO]->(dst:Entity {uuid: $dst}) \
             RETURN count(rn)",
            serde_json::json!({ "src": source_uuid, "name": name, "dst": target_uuid }),
        )?;
        if let Some(row) = rows.into_iter().next() {
            Ok(value_as_usize(&row[0]) > 0)
        } else {
            Ok(false)
        }
    }

    /// Returns a full RelatesToEdge by UUID, joining via the RelatesToNode_ shadow node.
    pub fn get_edge_by_uuid(&self, uuid: &str) -> Result<Option<RelatesToEdge>, Error> {
        let mut rows = self.collect_relates_to_edges(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.uuid = $uuid \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "uuid": uuid }),
        )?;
        Ok(rows.pop())
    }

    /// Returns all RELATES_TO edges where the entity with `entity_uuid` is either source or target.
    pub fn get_edges_for_entity(&self, entity_uuid: &str) -> Result<Vec<RelatesToEdge>, Error> {
        let mut edges = self.collect_relates_to_edges(
            "MATCH (src:Entity {uuid: $uuid})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "uuid": entity_uuid }),
        )?;
        edges.extend(self.collect_relates_to_edges(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity {uuid: $uuid}) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
            serde_json::json!({ "uuid": entity_uuid }),
        )?);
        Ok(edges)
    }

    /// Updates the labels array on the Entity with the given UUID.
    pub fn update_entity_labels(&self, uuid: &str, labels: &[String]) -> Result<(), Error> {
        self.exec_params(
            "MATCH (e:Entity {uuid: $uuid}) SET e.labels = $labels",
            serde_json::json!({ "uuid": uuid, "labels": labels }),
        )
    }

    /// Marks the edge identified by `edge_uuid` as invalid by setting `invalid_at`
    /// on the RelatesToNode_ shadow node. Also attempts to set `invalid_at` on the
    /// RELATES_TO relationship property (lbug 0.17.0 may not support SET on rels;
    /// if it fails the error is logged but not propagated).
    pub fn invalidate_edge(&self, edge_uuid: &str, invalid_at: &str) -> Result<(), Error> {
        // The `invalid_at` param name is timestamp-gated (see TIMESTAMP_PARAM_NAMES), so an
        // RFC-3339 value binds as a typed Timestamp — required for a `SET col = $x` assignment
        // into a TIMESTAMP column (lbug does not implicitly cast STRING→TIMESTAMP there).
        self.exec_params(
            "MATCH (rn:RelatesToNode_ {uuid: $uuid}) SET rn.invalid_at = $invalid_at",
            serde_json::json!({ "uuid": edge_uuid, "invalid_at": invalid_at }),
        )?;
        // Attempt SET on the RELATES_TO rel — non-fatal if unsupported.
        if let Err(e) = self.exec_params(
            "MATCH (src:Entity)-[r:RELATES_TO {uuid: $uuid}]->(dst:Entity) SET r.invalid_at = $invalid_at",
            serde_json::json!({ "uuid": edge_uuid, "invalid_at": invalid_at }),
        ) {
            eprintln!(
                "liminis-graph: SET invalid_at on RELATES_TO rel unsupported or failed (non-fatal): {e}"
            );
        }
        Ok(())
    }

    /// Returns a paged list of Entity nodes whose only label is the generic "Entity"
    /// (i.e., not yet classified into a specific type). Batch size 50 is `REPROCESS_BATCH_SIZE`.
    ///
    /// Uses `SKIP`/`LIMIT` for paging. `offset` is the number of rows to skip.
    pub fn list_generic_entities_page(
        &self,
        group_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<EntityRow>, Error> {
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.group_id = $gid AND size(e.labels) = 1 AND 'Entity' IN e.labels \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes ORDER BY e.uuid SKIP $offset LIMIT $limit",
            serde_json::json!({ "gid": group_id, "offset": offset as i64, "limit": limit as i64 }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                labels: value_as_str_list(&row[3]),
                created_at: value_as_timestamp_str(&row[4]),
                summary: value_as_string(&row[5]),
                attributes: value_as_string(&row[6]),
                ..Default::default()
            });
        }
        Ok(rows)
    }

    /// Returns entities whose name starts with `name_prefix`.
    /// Pass `""` to return all entities.
    pub fn search_entities(&self, name_prefix: &str) -> Result<Vec<EntityRow>, Error> {
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.name STARTS WITH $prefix \
             RETURN e.uuid, e.name, e.group_id, e.summary, e.attributes",
            serde_json::json!({ "prefix": name_prefix }),
        )?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(EntityRow {
                uuid: value_as_string(&row[0]),
                name: value_as_string(&row[1]),
                group_id: value_as_string(&row[2]),
                summary: value_as_string(&row[3]),
                attributes: value_as_string(&row[4]),
                ..Default::default()
            });
        }
        Ok(rows)
    }
}

// ── bound-parameter mapping ─────────────────────────────────────────────────────

/// Maps a JSON params object to lbug bound `(name, Value)` pairs for prepared-statement
/// execution. Type-agnostic by design: lbug coerces each bound value to its destination
/// column type, so we never need to know the schema here. (Empirically verified against
/// lbug 0.17: an RFC-3339 `String` binds into a `TIMESTAMP` column; a numeric list binds
/// into a `FLOAT[N]` column; a string list binds into a `STRING[]` column.)
///
/// A non-object params value (e.g. `Null`, as recorded by `raw_query` for DDL) yields no
/// bound params.
fn json_params_to_values(params: &serde_json::Value) -> Vec<(String, Value)> {
    let serde_json::Value::Object(map) = params else {
        return Vec::new();
    };
    map.iter()
        .map(|(k, v)| (k.clone(), json_value_for_param(k, v)))
        .collect()
}

/// Parameter names that target `TIMESTAMP` columns. Only these bind an RFC-3339 string as a
/// typed `Value::Timestamp`; every other string binds verbatim as `Value::String`.
///
/// This gate is the fix for the corruption where a value-shape sniff would rewrite any
/// timestamp-looking string — including user content in STRING columns (`content`, `summary`,
/// `fact`, `name`) — into a space-formatted timestamp. Timestamp coercion is needed because
/// lbug does not implicitly cast `STRING`→`TIMESTAMP` in a `SET col = $x` assignment (it does
/// in a CREATE property map); gating by the destination column's param name makes the coercion
/// column-aware. `timestamp($x)`-wrapped templates also accept a typed Timestamp (idempotent),
/// so this is safe for both bare and wrapped WAL forms.
const TIMESTAMP_PARAM_NAMES: &[&str] = &["created_at", "valid_at", "invalid_at", "expired_at"];

/// Maps a JSON param `(name, value)` to an lbug `Value`, applying timestamp typing only to
/// known timestamp-column param names (see `TIMESTAMP_PARAM_NAMES`).
fn json_value_for_param(key: &str, v: &serde_json::Value) -> Value {
    if TIMESTAMP_PARAM_NAMES.contains(&key) {
        if let serde_json::Value::String(s) = v {
            if let Ok(odt) =
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            {
                return Value::Timestamp(odt);
            }
            // A non-RFC-3339 timestamp string (e.g. space-format from the Python WAL) binds as
            // a plain String; those templates wrap it with `timestamp($x)`, which parses it.
        }
    }
    json_to_value(v)
}

/// Converts a single JSON value to an lbug `Value`. Numeric arrays are forced to `Double`
/// children (embeddings are floats; lbug coerces `Double`→`Float` into `FLOAT[N]`), which
/// also avoids a heterogeneous int/float list when an embedding contains an exact `0`.
fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null(LogicalType::Any),
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int64(i)
            } else {
                Value::Double(n.as_f64().unwrap_or(0.0))
            }
        }
        // Strings bind verbatim. Timestamp typing is applied upstream in `json_value_for_param`,
        // gated on the destination column's param name — never on value shape — so user content
        // that happens to look like a timestamp is never rewritten.
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => match arr.first() {
            Some(serde_json::Value::Number(_)) => Value::List(
                LogicalType::Double,
                arr.iter()
                    .map(|x| Value::Double(x.as_f64().unwrap_or(0.0)))
                    .collect(),
            ),
            Some(serde_json::Value::String(_)) => Value::List(
                LogicalType::String,
                arr.iter()
                    .map(|x| Value::String(x.as_str().unwrap_or_default().to_string()))
                    .collect(),
            ),
            Some(_) => {
                let child = logical_type_of(&arr[0]);
                Value::List(child, arr.iter().map(json_to_value).collect())
            }
            // Empty list: default to STRING[] — the only plausibly-empty array columns are
            // STRING[] (episodes, labels, entity_edges); embeddings are always populated.
            None => Value::List(LogicalType::String, Vec::new()),
        },
        // Nested objects are rare in our params; bind as JSON so lbug can store/coerce.
        serde_json::Value::Object(_) => Value::Json(v.clone()),
    }
}

fn logical_type_of(v: &serde_json::Value) -> LogicalType {
    match v {
        serde_json::Value::Bool(_) => LogicalType::Bool,
        serde_json::Value::Number(n) => {
            if n.as_i64().is_some() {
                LogicalType::Int64
            } else {
                LogicalType::Double
            }
        }
        _ => LogicalType::String,
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn value_as_string(v: &lbug::Value) -> String {
    match v {
        lbug::Value::String(s) => s.clone(),
        lbug::Value::Null(_) => String::new(),
        _ => v.to_string(),
    }
}

fn value_as_timestamp_str(v: &lbug::Value) -> String {
    match v {
        lbug::Value::Timestamp(dt) => format_datetime(*dt),
        lbug::Value::String(s) => s.clone(),
        lbug::Value::Null(_) => String::new(),
        _ => v.to_string(),
    }
}

fn value_as_optional_timestamp_str(v: &lbug::Value) -> Option<String> {
    match v {
        lbug::Value::Null(_) => None,
        other => Some(value_as_timestamp_str(other)),
    }
}

fn value_as_optional_string(v: &lbug::Value) -> Option<String> {
    match v {
        lbug::Value::Null(_) => None,
        lbug::Value::String(s) if s.is_empty() => None,
        other => Some(value_as_string(other)),
    }
}

fn value_as_f64(v: &lbug::Value) -> f64 {
    match v {
        lbug::Value::Double(f) => *f,
        lbug::Value::Float(f) => *f as f64,
        lbug::Value::Int64(i) => *i as f64,
        _ => 0.0,
    }
}

fn value_as_usize(v: &lbug::Value) -> usize {
    match v {
        lbug::Value::Int64(i) => *i as usize,
        lbug::Value::UInt64(i) => *i as usize,
        lbug::Value::Int32(i) => *i as usize,
        lbug::Value::Double(f) => *f as usize,
        _ => 0,
    }
}

fn value_as_float_array(v: &lbug::Value) -> Vec<f32> {
    match v {
        lbug::Value::Array(_, elems) | lbug::Value::List(_, elems) => elems
            .iter()
            .map(|e| match e {
                lbug::Value::Float(f) => *f,
                lbug::Value::Double(f) => *f as f32,
                _ => 0.0,
            })
            .collect(),
        _ => vec![],
    }
}

fn value_as_str_list(v: &lbug::Value) -> Vec<String> {
    match v {
        lbug::Value::Array(_, elems) | lbug::Value::List(_, elems) => {
            elems.iter().map(value_as_string).collect()
        }
        _ => vec![],
    }
}

fn format_datetime(dt: time::OffsetDateTime) -> String {
    // Format as "YYYY-MM-DD HH:MM:SS" (matches Python graphiti-core wire format)
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn format_datetime_iso8601(dt: time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn enforce_entity_first(labels: &[String]) -> Vec<String> {
    if labels.first().map(String::as_str) == Some("Entity") {
        return labels.to_vec();
    }
    let mut out = vec!["Entity".to_string()];
    for l in labels {
        if l != "Entity" {
            out.push(l.clone());
        }
    }
    out
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod relates_to_merge_repro {
    use super::*;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        (dir, db)
    }

    /// Applies the replay-time legacy normalization (`strip_vecf32` + bulk-`SET` expansion) the
    /// way `WalReplayer` does before `prepare()`, so these tests feed `prepare()` the exact
    /// template the replay would.
    fn normalize(raw: &str) -> String {
        let n = crate::legacy_wal::strip_vecf32(raw);
        let (n, _p) = crate::legacy_wal::expand_bulk_property_set(&n, serde_json::json!({}));
        n
    }

    /// Regression for the MENTIONS schema gap. graphiti's MENTIONS edge carries `uuid` and
    /// `created_at` on the relationship, but liminis-graph's MENTIONS rel table previously
    /// declared only `group_id`. As a result this WAL statement failed to `prepare()` with
    /// `Binder exception: Cannot find property uuid for r`, and the batched replay then
    /// classified *every* MENTIONS mutation sharing the template as failed — silently dropping
    /// the entire episode→entity mention layer. With `uuid`/`created_at` added it must prepare.
    #[test]
    fn mentions_edge_merge_prepares_against_real_schema() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        conn.init_schema(768).unwrap();
        let cypher = "MATCH (src:Episodic {uuid: $src_uuid}) \
             MATCH (dst:Entity {uuid: $dst_uuid}) \
             MERGE (src)-[r:MENTIONS {uuid: $uuid}]->(dst) \
             SET r.group_id = $group_id, r.created_at = $created_at";
        let res = conn.prepare(&normalize(cypher));
        assert!(
            res.is_ok(),
            "MENTIONS edge MERGE must prepare after the schema fix; got: {:?}",
            res.err()
        );
    }

    /// Guard: the reified-edge (`RelatesToNode_`) two-hop MERGE — the dominant edge write —
    /// must `prepare()` against the real schema after `strip_vecf32` normalization. Uses the
    /// exact WAL shape (two `SET` clauses + a `vecf32(...)` embedding wrapper).
    #[test]
    fn relates_to_two_hop_merge_prepares_against_real_schema() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        conn.init_schema(768).unwrap();
        let cypher = "MATCH (src:Entity {uuid: $src_uuid}) \
             MATCH (dst:Entity {uuid: $dst_uuid}) \
             MERGE (src)-[:RELATES_TO]->(r:RelatesToNode_ {uuid: $uuid})-[:RELATES_TO]->(dst) \
             SET r.name = $name, r.fact = $fact, r.group_id = $group_id, r.episodes = $episodes, \
             r.created_at = $created_at, r.valid_at = $valid_at \
             SET r.fact_embedding = vecf32($fact_embedding)";
        let res = conn.prepare(&normalize(cypher));
        assert!(
            res.is_ok(),
            "reified-edge two-hop MERGE must prepare against the real schema; got: {:?}",
            res.err()
        );
    }

    /// Regression for the missing community/saga stub tables. graphiti's bulk edge-delete lists
    /// multiple rel types incl. HAS_MEMBER; before the stubs the missing HAS_MEMBER table made the
    /// whole multi-type pattern fail to prepare (`Table HAS_MEMBER does not exist`), silently
    /// skipping the MENTIONS/RELATES_TO deletes too. With the stub tables present it must prepare.
    #[test]
    fn multi_type_edge_delete_prepares_with_stub_tables() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        conn.init_schema(768).unwrap();
        let cypher =
            "MATCH (n)-[e:MENTIONS|RELATES_TO|HAS_MEMBER]->(m) WHERE e.uuid IN $uuids DELETE e";
        let res = conn.prepare(cypher);
        assert!(
            res.is_ok(),
            "multi-type edge DELETE must prepare with stub tables present; got: {:?}",
            res.err()
        );
    }
}

#[cfg(test)]
mod fts_missing_index_tests {
    use super::*;
    use tempfile::TempDir;

    /// Regression: lbug 0.17 returns a "Binder exception: ... doesn't have an index with name"
    /// error for both HNSW *and* FTS missing indexes. is_missing_index_error already matches
    /// both cases — this test guards against future lbug versions changing the error text for FTS.
    #[test]
    fn fts_missing_index_error_matches_binder_exception() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("fts_probe.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        crate::schema::drop_fts_indexes(&conn);
        conn.insert_entity(&crate::EntityRow {
            uuid: "fts-probe-1".to_string(),
            name: "FtsProbeEntity".to_string(),
            group_id: "g".to_string(),
            labels: vec![],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0f32; 4],
            summary: "probe".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        let err = conn
            .fts_search_entities("probe", &["g"], 5)
            .expect_err("should fail with missing FTS index");
        let msg = err.to_string();
        assert!(
            msg.contains("Binder exception:") && msg.contains("doesn't have an index with name"),
            "FTS missing-index error must match the same pattern as HNSW — got: {msg}"
        );
    }
}
