use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

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
    /// correctly. See `wal_exec.rs` for the drain-and-flush pattern (ADR-0015).
    executed_mutations: RefCell<Vec<(String, serde_json::Value)>>,
}

/// Serializes `Db::open` across threads. `INSTALL`/`LOAD EXTENSION` mutate a
/// *process-global* extension install location (not per-Database), so two
/// threads opening fresh Databases concurrently race that shared state and can
/// segfault the lbug C++ engine — a Linux-specific, schedule-sensitive crash
/// that surfaces under parallel `cargo test` (each test opens its own temp DB).
/// Production opens exactly one Database per service, so this lock is
/// contention-free outside the test suite. Poison-tolerant: a panic mid-open
/// must not wedge every other open.
static OPEN_LOCK: Mutex<()> = Mutex::new(());

impl Db {
    pub fn open(path: &str) -> Result<Self, Error> {
        let _open_guard = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let inner = lbug::Database::new(path, lbug::SystemConfig::default())?;
        // Both INSTALL and LOAD EXTENSION are write transactions in lbug,
        // and both must run before any vector / FTS use. Extensions persist at
        // the Database level (not per-Connection), so we set them up once here
        // — running them in connect() races concurrent callers. The OPEN_LOCK
        // above additionally serializes this block across Databases.
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

        // Direct Entity→Entity rel. All deployments use the canonical TIMESTAMP schema;
        // the former "non-fatal" catch for Python-schema DBs is removed.
        self.exec_params(
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
        )?;

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

    /// Creates HNSW vector indexes on Entity and Episodic. Idempotent — an "already exists"
    /// error (e.g. a repeat call, or one following `init_schema`) is swallowed; any other
    /// error (missing table, malformed column, ...) propagates so callers can observe a
    /// genuine index-build failure instead of silently treating it as success.
    pub fn create_vector_indexes(&self) -> Result<(), Error> {
        for sql in [
            "CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', \
             'name_embedding', metric := 'cosine')",
            "CALL CREATE_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', \
             'content_embedding', metric := 'cosine')",
            "CALL CREATE_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx', \
             'fact_embedding', metric := 'cosine')",
        ] {
            if let Err(e) = self.raw_query(sql) {
                if !crate::error::is_already_exists_error(&e) {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Drops the 3 HNSW vector indexes. Idempotent — errors (including "no such index") are
    /// suppressed so this is safe to call even when the indexes are already absent. Used by
    /// `handle_rebuild_from_wal` to ensure a from-scratch rebuild doesn't leave a stale
    /// pre-rebuild HNSW index in place after `CREATE_VECTOR_INDEX` stops treating every error
    /// as "already exists, move on."
    pub fn drop_vector_indexes(&self) {
        let _ = self.raw_query("CALL DROP_VECTOR_INDEX('Entity', 'entity_name_embedding_idx')");
        let _ =
            self.raw_query("CALL DROP_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx')");
        let _ =
            self.raw_query("CALL DROP_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx')");
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

    /// Returns an EntityRow by case-insensitive, whitespace-normalised name match.
    ///
    /// Input name is trimmed and lowercased in Rust before being passed as the `$lower_name`
    /// parameter. The query also applies `lower()` to the stored name so that entities
    /// inserted with mixed-case names are found. Returns the first match if multiple exist.
    pub fn get_entity_by_name_ci(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Option<EntityRow>, Error> {
        let lower_name = name.trim().to_lowercase();
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE lower(e.name) = $lower_name AND e.group_id = $gid \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes ORDER BY e.created_at ASC, e.uuid ASC LIMIT 1",
            serde_json::json!({ "lower_name": lower_name, "gid": group_id }),
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

    /// Counts Entity nodes whose lowercased name matches the given name (case-insensitive)
    /// within a group. Primarily used in tests for asserting dedup correctness.
    pub fn count_entities_by_name_ci(&self, name: &str, group_id: &str) -> Result<usize, Error> {
        let lower_name = name.trim().to_lowercase();
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE lower(e.name) = $lower_name AND e.group_id = $gid \
             RETURN count(e)",
            serde_json::json!({ "lower_name": lower_name, "gid": group_id }),
        )?;
        Ok(rows
            .into_iter()
            .next()
            .map(|r| value_as_usize(&r[0]))
            .unwrap_or(0))
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

    /// Returns ALL entities with the given `name` in `group_id`, ordered by
    /// `created_at ASC, uuid ASC`. Unlike `get_entity_by_name`, this method has no `LIMIT 1`
    /// and returns every matching node — used by `merge_entities` for canonical selection
    /// and alias expansion.
    pub fn get_entities_by_name_all(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Vec<EntityRow>, Error> {
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE e.name = $name AND e.group_id = $gid \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes ORDER BY e.created_at ASC, e.uuid ASC",
            serde_json::json!({ "name": name, "gid": group_id }),
        )?;
        let mut result = Vec::new();
        for row in rows {
            result.push(EntityRow {
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
        Ok(result)
    }

    /// Sets `created_at` on the Entity with `uuid` to `created_at`.
    /// Must use `timestamp($new_created_at)` in Cypher because lbug requires the `timestamp()`
    /// function when assigning a string value to a TIMESTAMP column in a SET clause (bare
    /// `SET col = $x` with a string binds fail; see ADR-0024).
    /// The param is named `new_created_at` (not `created_at`) to bypass TIMESTAMP_PARAM_NAMES
    /// auto-coercion: the input is always a space-format string ("YYYY-MM-DD HH:MM:SS") from
    /// the DB, and we want `timestamp()` to receive it as a string — the natural, unambiguous
    /// path. (`timestamp(Value::Timestamp)` is also accepted by lbug and is idempotent, as
    /// confirmed by dump-replay tests, but the rename keeps the intent explicit.)
    pub fn update_entity_created_at(&self, uuid: &str, created_at: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (e:Entity {uuid: $uuid}) SET e.created_at = timestamp($new_created_at)",
            serde_json::json!({ "uuid": uuid, "new_created_at": created_at }),
        )
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

    pub fn count_mentions_edges(&self) -> Result<u64, Error> {
        let result = self
            .inner
            .query("MATCH ()-[r:MENTIONS]->() RETURN count(*)")?;
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
             WHERE rn.invalid_at IS NULL \
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
                "liminis-context-graph: SET invalid_at on RELATES_TO rel unsupported or failed (non-fatal): {e}"
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

    /// Returns a paged list of Entity nodes that carry at least one specific type label
    /// (i.e., `size(labels) >= 2`). Phase D inspects these to add missing ancestor labels,
    /// covering both nodes that never had hierarchy (`["Entity", "Rfc"]`) and nodes whose
    /// ancestor labels are stale after a hierarchy change (`["Entity", "Document", "Rfc"]`).
    pub fn list_typed_entities_page(
        &self,
        group_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<EntityRow>, Error> {
        let result = self.query_params(
            "MATCH (e:Entity) WHERE e.group_id = $gid AND size(e.labels) >= 2 AND 'Entity' IN e.labels \
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

    // ── dump/compaction query methods ─────────────────────────────────────────
    // Used exclusively by dump.rs for `knowledge_dump_wal`. Return raw column vectors so
    // dump.rs can access embedding values without extra allocations.
    //
    // Column ordering is fixed; dump.rs uses named const indices to avoid magic numbers.

    /// Page of Entity rows for dump.
    /// Columns: [uuid, name, group_id, labels, created_at, name_embedding, summary, attributes]
    pub(crate) fn dump_entities_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (n:Entity) WHERE n.group_id = $gid \
                 RETURN n.uuid, n.name, n.group_id, n.labels, n.created_at, \
                 n.name_embedding, n.summary, n.attributes \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (n:Entity) \
                 RETURN n.uuid, n.name, n.group_id, n.labels, n.created_at, \
                 n.name_embedding, n.summary, n.attributes \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of Episodic rows for dump.
    /// Columns: [uuid, name, group_id, created_at, source, source_description, content,
    ///            content_embedding, valid_at, entity_edges]
    pub(crate) fn dump_episodics_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (n:Episodic) WHERE n.group_id = $gid \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.source, \
                 n.source_description, n.content, n.content_embedding, n.valid_at, n.entity_edges \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (n:Episodic) \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.source, \
                 n.source_description, n.content, n.content_embedding, n.valid_at, n.entity_edges \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of RelatesToNode_ rows for dump.
    /// Columns: [uuid, name, group_id, created_at, fact, fact_embedding, episodes,
    ///            expired_at, valid_at, invalid_at, attributes, relation_type]
    pub(crate) fn dump_relatos_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (n:RelatesToNode_) WHERE n.group_id = $gid \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.fact, \
                 n.fact_embedding, n.episodes, n.expired_at, n.valid_at, n.invalid_at, \
                 n.attributes, n.relation_type \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (n:RelatesToNode_) \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.fact, \
                 n.fact_embedding, n.episodes, n.expired_at, n.valid_at, n.invalid_at, \
                 n.attributes, n.relation_type \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of Community rows for dump.
    /// Columns: [uuid, name, group_id, created_at, name_embedding, summary]
    pub(crate) fn dump_community_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (n:Community) WHERE n.group_id = $gid \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.name_embedding, n.summary \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (n:Community) \
                 RETURN n.uuid, n.name, n.group_id, n.created_at, n.name_embedding, n.summary \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of Saga rows for dump.
    /// Columns: [uuid, name, group_id, created_at]
    pub(crate) fn dump_saga_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (n:Saga) WHERE n.group_id = $gid \
                 RETURN n.uuid, n.name, n.group_id, n.created_at \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (n:Saga) \
                 RETURN n.uuid, n.name, n.group_id, n.created_at \
                 ORDER BY n.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of RELATES_TO two-hop links for dump (src→RelatesToNode_→dst pattern).
    /// Columns: [src_uuid, rn_uuid, dst_uuid]
    pub(crate) fn dump_relates_to_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 WHERE rn.group_id = $gid \
                 RETURN src.uuid, rn.uuid, dst.uuid \
                 ORDER BY rn.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 RETURN src.uuid, rn.uuid, dst.uuid \
                 ORDER BY rn.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of MENTIONS edges for dump.
    /// Columns: [ep_uuid, en_uuid, r_uuid, r_group_id, r_created_at]
    /// Rows with null r_uuid must be skipped by the caller (pre-migration edges).
    pub(crate) fn dump_mentions_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (ep:Episodic)-[r:MENTIONS]->(en:Entity) WHERE r.group_id = $gid \
                 RETURN ep.uuid, en.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY ep.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (ep:Episodic)-[r:MENTIONS]->(en:Entity) \
                 RETURN ep.uuid, en.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY ep.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of HAS_EPISODE edges (Saga→Episodic) for dump.
    /// Columns: [sg_uuid, ep_uuid, r_uuid, r_group_id, r_created_at]
    pub(crate) fn dump_has_episode_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (sg:Saga)-[r:HAS_EPISODE]->(ep:Episodic) WHERE r.group_id = $gid \
                 RETURN sg.uuid, ep.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY sg.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (sg:Saga)-[r:HAS_EPISODE]->(ep:Episodic) \
                 RETURN sg.uuid, ep.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY sg.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of HAS_MEMBER edges (Community→Entity) for dump.
    /// Columns: [c_uuid, e_uuid, r_uuid, r_group_id, r_created_at]
    pub(crate) fn dump_has_member_entity_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (c:Community)-[r:HAS_MEMBER]->(e:Entity) WHERE r.group_id = $gid \
                 RETURN c.uuid, e.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY c.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (c:Community)-[r:HAS_MEMBER]->(e:Entity) \
                 RETURN c.uuid, e.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY c.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of HAS_MEMBER edges (Community→Community) for dump.
    /// Columns: [c_uuid, m_uuid, r_uuid, r_group_id, r_created_at]
    pub(crate) fn dump_has_member_community_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (c:Community)-[r:HAS_MEMBER]->(m:Community) WHERE r.group_id = $gid \
                 RETURN c.uuid, m.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY c.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (c:Community)-[r:HAS_MEMBER]->(m:Community) \
                 RETURN c.uuid, m.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY c.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
    }

    /// Page of NEXT_EPISODE edges (Episodic→Episodic) for dump.
    /// Columns: [ep1_uuid, ep2_uuid, r_uuid, r_group_id, r_created_at]
    pub(crate) fn dump_next_episode_page(
        &self,
        group_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Vec<lbug::Value>>, Error> {
        if let Some(gid) = group_id {
            self.query_params(
                "MATCH (ep1:Episodic)-[r:NEXT_EPISODE]->(ep2:Episodic) WHERE r.group_id = $gid \
                 RETURN ep1.uuid, ep2.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY ep1.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "gid": gid, "offset": offset as i64, "limit": limit as i64 }),
            )
        } else {
            self.query_params(
                "MATCH (ep1:Episodic)-[r:NEXT_EPISODE]->(ep2:Episodic) \
                 RETURN ep1.uuid, ep2.uuid, r.uuid, r.group_id, r.created_at \
                 ORDER BY ep1.uuid SKIP $offset LIMIT $limit",
                serde_json::json!({ "offset": offset as i64, "limit": limit as i64 }),
            )
        }
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

/// Parameter names that target `TIMESTAMP` columns. Only these bind an RFC-3339 or space-format
/// string as a typed `Value::Timestamp`; every other string binds verbatim as `Value::String`.
///
/// This gate prevents value-shape sniffing from rewriting user content in STRING columns
/// (`content`, `summary`, `fact`, `name`) into timestamps. Explicit coercion is required because
/// lbug does not implicitly cast `STRING`→`TIMESTAMP` in `SET col = $x` assignments (it does in
/// CREATE property maps); gating by param name makes coercion column-aware.
/// `timestamp($x)`-wrapped templates also accept a typed Timestamp (idempotent).
///
/// # Write-path inventory (Issue #170)
///
/// Every write path that sends data to lbug is listed below with its coercion status. When you
/// add a new write path or a new TIMESTAMP column to the schema:
///   - If the Cypher uses bare `SET col = $param` → add the param name to `TIMESTAMP_PARAM_NAMES`
///   - If the Cypher uses `timestamp($param)` or `CASE WHEN … THEN NULL ELSE timestamp($param) END`
///     → the Cypher wrapper handles coercion; no change to `TIMESTAMP_PARAM_NAMES` needed
///   - NEVER interpolate a timestamp string directly into a Cypher literal — always use bound params
///
/// | Write path                              | Method           | Status                      |
/// |-----------------------------------------|------------------|-----------------------------|
/// | `insert_entity`                         | `exec_params`    | ✓ exec_params gate          |
/// | `insert_episodic`                       | `exec_params`    | ✓ exec_params gate          |
/// | `insert_relates_to_edge`                | `exec_params`    | ✓ exec_params gate          |
/// | `insert_mentions_edge`                  | `exec_params`    | ✓ exec_params gate          |
/// | `invalidate_edge`                       | `exec_params`    | ✓ exec_params gate          |
/// | `update_entity_labels`                  | `exec_params`    | ✓ no timestamp fields       |
/// | `update_entity_created_at`              | `exec_params`    | ✓ Cypher wrapper (see note) |
/// | `corrections::apply_same_as`            | via insert/inval | ✓ exec_params gate          |
/// | `corrections::apply_retract`            | via invalidate   | ✓ exec_params gate          |
/// | `corrections::apply_entity_type_labels` | via update_labels| ✓ no timestamp fields       |
/// | `merge_entities`                        | via insert/inval | ✓ exec_params gate          |
/// | `dump.rs` (all node/edge types)         | `WalWriter`      | ✓ RFC-3339+µs WAL; Cypher   |
/// |                                         |                  |   wrapper coerces on replay |
/// | `knowledge_query_cypher`                | `cypher_query`   | safe — raw Cypher, no param |
/// |                                         |                  | interpolation (FR-008)      |
/// | Relation canonicalization (#163)        | not yet impl.    | deferred — #163 pending     |
///
/// Note: `update_entity_created_at` uses param name `$new_created_at` (NOT in this list) plus a
/// `timestamp($new_created_at)` Cypher wrapper. This is intentional — the value arrives as a
/// space-format string from `value_as_timestamp_str` and the wrapper handles coercion. Do NOT add
/// `new_created_at` to this list; doing so would double-apply coercion and break the path.
const TIMESTAMP_PARAM_NAMES: &[&str] = &["created_at", "valid_at", "invalid_at", "expired_at"];

/// Maps a JSON param `(name, value)` to an lbug `Value`, applying timestamp typing only to
/// known timestamp-column param names (see `TIMESTAMP_PARAM_NAMES`).
///
/// Accepts two timestamp formats:
/// - RFC-3339 (e.g. `"2026-06-01T12:00:00Z"`) — produced by the WAL write path.
/// - Space format (e.g. `"2026-06-01 00:00:00"`) — produced by `value_as_timestamp_str` when
///   reading timestamps back from lbug. Parsed as UTC. This covers the merge round-trip path
///   where edges are read from the DB and re-inserted via `insert_relates_to_edge`.
fn json_value_for_param(key: &str, v: &serde_json::Value) -> Value {
    if TIMESTAMP_PARAM_NAMES.contains(&key) {
        if let serde_json::Value::String(s) = v {
            if let Ok(odt) =
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            {
                return Value::Timestamp(odt);
            }
            // Space-format "YYYY-MM-DD HH:MM:SS" — DB read-back format. Assumed UTC.
            const SPACE_FMT: &[time::format_description::FormatItem<'static>] =
                time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
            if let Ok(pdt) = time::PrimitiveDateTime::parse(s, SPACE_FMT) {
                return Value::Timestamp(pdt.assume_utc());
            }
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

pub(crate) fn value_as_string(v: &lbug::Value) -> String {
    match v {
        lbug::Value::String(s) => s.clone(),
        lbug::Value::Null(_) => String::new(),
        _ => v.to_string(),
    }
}

pub(crate) fn value_as_timestamp_str(v: &lbug::Value) -> String {
    match v {
        lbug::Value::Timestamp(dt) => format_datetime(*dt),
        lbug::Value::String(s) => s.clone(),
        lbug::Value::Null(_) => String::new(),
        _ => v.to_string(),
    }
}

pub(crate) fn value_as_optional_timestamp_str(v: &lbug::Value) -> Option<String> {
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

pub(crate) fn value_as_float_array(v: &lbug::Value) -> Vec<f32> {
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

pub(crate) fn value_as_str_list(v: &lbug::Value) -> Vec<String> {
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

/// Formats an `OffsetDateTime` as RFC-3339 with exactly 6 fractional-second digits (microseconds).
///
/// Used by the WAL dump path to preserve sub-second precision through dump→wipe→replay cycles.
/// Always emits `YYYY-MM-DDTHH:MM:SS.ffffffZ` — exactly 6 digits — regardless of the
/// nanosecond remainder, so the format is stable and predictable for the replay-time parser.
///
/// Do NOT use this for IPC responses: the Python layer expects the space-format produced by
/// `format_datetime`. This function is dump-path-only.
pub(crate) fn format_datetime_rfc3339_subsecond(dt: time::OffsetDateTime) -> String {
    // Convert to UTC so the hardcoded 'Z' suffix is correct even if dt carries a non-UTC offset.
    let dt = dt.to_offset(time::UtcOffset::UTC);
    // Kuzu stores TIMESTAMP with microsecond precision; truncate nanoseconds.
    let microseconds = dt.microsecond();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        microseconds
    )
}

/// Normalizes a timestamp string (RFC-3339 or space-format) to RFC-3339 with microseconds.
///
/// Used by the WAL dump path to ensure that TIMESTAMP columns stored as strings (e.g., a
/// read-back from lbug via `Value::String`) are re-emitted with the same format and precision
/// as columns returned as `Value::Timestamp`. Falls through verbatim if neither format parses.
pub(crate) fn normalize_ts_str_for_dump(s: &str) -> String {
    if let Ok(odt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
    {
        return format_datetime_rfc3339_subsecond(odt);
    }
    const SPACE_FMT: &[time::format_description::FormatItem<'static>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    if let Ok(pdt) = time::PrimitiveDateTime::parse(s, SPACE_FMT) {
        return format_datetime_rfc3339_subsecond(pdt.assume_utc());
    }
    s.to_string()
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

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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

#[cfg(test)]
mod create_vector_indexes_tests {
    use super::*;
    use tempfile::TempDir;

    /// Regression guard for issue #192: `create_vector_indexes` must stay idempotent — a second
    /// back-to-back call (e.g. a repeat `knowledge_build_indices`, or the post-reload build
    /// following `init_schema`'s own index creation) must swallow the "already exists" error
    /// and return `Ok(())`, not propagate it as a genuine failure.
    #[test]
    fn double_create_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        // init_schema already created the indexes once; call again explicitly twice more.
        assert!(conn.create_vector_indexes().is_ok());
        assert!(conn.create_vector_indexes().is_ok());
    }

    /// Regression guard for issue #192: a genuine failure (target table missing) must propagate
    /// as `Err`, not be silently swallowed as "already exists". Before the fix,
    /// `create_vector_indexes` blanket-suppressed every error and always returned `Ok(())`.
    #[test]
    fn missing_table_returns_genuine_error() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        // No init_schema() — Entity/Episodic/RelatesToNode_ tables don't exist.
        let err = conn
            .create_vector_indexes()
            .expect_err("must fail when target tables don't exist");
        assert!(
            !crate::error::is_already_exists_error(&err),
            "missing-table error must not be misclassified as already-exists: {err}"
        );
    }
}

// ── FR-009: unit tests for json_value_for_param and json_to_value ─────────────
#[cfg(test)]
mod coerce_unit_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rfc3339_timestamp_param_coerced_to_value_timestamp() {
        let v = json_value_for_param("created_at", &json!("2024-01-15T10:30:00Z"));
        assert!(
            matches!(v, Value::Timestamp(_)),
            "RFC-3339 created_at must yield Value::Timestamp, got: {v:?}"
        );
    }

    #[test]
    fn space_format_timestamp_param_coerced_to_value_timestamp() {
        let v = json_value_for_param("created_at", &json!("2024-01-15 10:30:00"));
        assert!(
            matches!(v, Value::Timestamp(_)),
            "space-format created_at must yield Value::Timestamp, got: {v:?}"
        );
    }

    #[test]
    fn rfc3339_string_in_non_timestamp_column_stays_string() {
        let v = json_value_for_param("name", &json!("2024-01-15T10:30:00Z"));
        assert!(
            matches!(v, Value::String(_)),
            "datetime-looking string in 'name' column must stay Value::String, got: {v:?}"
        );
    }

    #[test]
    fn float_array_becomes_double_list() {
        let v = json_to_value(&json!([0.1, 0.2, 0.3]));
        match &v {
            Value::List(lt, elems) => {
                assert_eq!(
                    *lt,
                    LogicalType::Double,
                    "float array must use Double child type"
                );
                assert_eq!(elems.len(), 3, "element count must match");
                assert!(
                    matches!(elems[0], Value::Double(_)),
                    "elements must be Value::Double"
                );
            }
            other => panic!("expected Value::List, got: {other:?}"),
        }
    }

    #[test]
    fn null_becomes_value_null_any() {
        let v = json_to_value(&json!(null));
        assert!(
            matches!(v, Value::Null(LogicalType::Any)),
            "json null must yield Value::Null(Any), got: {v:?}"
        );
    }

    #[test]
    fn apostrophe_string_binds_verbatim() {
        let v = json_to_value(&json!("O'Brien"));
        match v {
            Value::String(s) => assert_eq!(s, "O'Brien", "apostrophe must be preserved verbatim"),
            other => panic!("expected Value::String, got: {other:?}"),
        }
    }

    #[test]
    fn integer_becomes_int64() {
        let v = json_to_value(&json!(42));
        assert!(
            matches!(v, Value::Int64(42)),
            "integer 42 must yield Value::Int64(42), got: {v:?}"
        );
    }

    #[test]
    fn bool_becomes_value_bool() {
        let v_true = json_to_value(&json!(true));
        let v_false = json_to_value(&json!(false));
        assert!(
            matches!(v_true, Value::Bool(true)),
            "true must yield Value::Bool(true), got: {v_true:?}"
        );
        assert!(
            matches!(v_false, Value::Bool(false)),
            "false must yield Value::Bool(false), got: {v_false:?}"
        );
    }
}
