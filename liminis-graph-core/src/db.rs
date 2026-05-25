use std::collections::HashMap;
use std::path::Path;

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
        Ok(Conn { inner: conn })
    }
}

impl<'db> Conn<'db> {
    /// Runs a raw Cypher statement returning no rows; used internally and for testing.
    pub(crate) fn raw_query(&self, sql: &str) -> Result<(), Error> {
        let _ = self.inner.query(sql)?;
        Ok(())
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
    pub fn cypher_query(&self, sql: &str) -> Result<Vec<Vec<String>>, Error> {
        let result = self.inner.query(sql)?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(row.iter().map(value_as_string).collect());
        }
        Ok(rows)
    }

    /// Creates the Entity and Episodic node tables. Call once after connecting.
    pub fn init_schema(&self, embedding_dim: usize) -> Result<(), Error> {
        crate::schema::init(self, embedding_dim)
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
        let sql = format!(
            "CREATE (:Entity {{uuid: '{}', name: '{}', group_id: '{}', labels: {}, \
             created_at: timestamp('{}'), name_embedding: {}, summary: '{}', \
             attributes: '{}'}})",
            escape(&row.uuid),
            escape(&row.name),
            escape(&row.group_id),
            format_str_array(&labels),
            escape(&row.created_at),
            format_float_array(&row.name_embedding),
            escape(&row.summary),
            escape(&row.attributes),
        );
        self.raw_query(&sql)
    }

    pub fn insert_episodic(&self, row: &EpisodicRow) -> Result<(), Error> {
        let sql = format!(
            "CREATE (:Episodic {{uuid: '{}', name: '{}', group_id: '{}', \
             created_at: timestamp('{}'), source: '{}', source_description: '{}', \
             content: '{}', content_embedding: {}, valid_at: timestamp('{}'), \
             entity_edges: {}}})",
            escape(&row.uuid),
            escape(&row.name),
            escape(&row.group_id),
            escape(&row.created_at),
            escape(&row.source),
            escape(&row.source_description),
            escape(&row.content),
            format_float_array(&row.content_embedding),
            escape(&row.valid_at),
            format_str_array(&row.entity_edges),
        );
        self.raw_query(&sql)
    }

    // ── Edge insert ───────────────────────────────────────────────────────────

    /// Inserts a RELATES_TO rel edge and the corresponding RelatesToNode_ shadow node.
    pub fn insert_relates_to_edge(&self, edge: &RelatesToEdge) -> Result<(), Error> {
        // Insert shadow node for vector search
        let valid_at = edge
            .valid_at
            .as_deref()
            .map(|t| format!("timestamp('{}')", escape(t)))
            .unwrap_or_else(|| "null".to_string());
        let invalid_at = edge
            .invalid_at
            .as_deref()
            .map(|t| format!("timestamp('{}')", escape(t)))
            .unwrap_or_else(|| "null".to_string());

        let node_sql = format!(
            "CREATE (:RelatesToNode_ {{uuid: '{}', name: '{}', group_id: '{}', \
             created_at: timestamp('{}'), fact: '{}', fact_embedding: {}, \
             valid_at: {valid_at}, invalid_at: {invalid_at}, attributes: '{}'}})",
            escape(&edge.uuid),
            escape(&edge.name),
            escape(&edge.group_id),
            escape(&edge.created_at),
            escape(&edge.fact),
            format_float_array(&edge.fact_embedding),
            escape(&edge.attributes),
        );
        self.raw_query(&node_sql)?;

        // Insert RELATES_TO rel between source and target Entity nodes
        let rel_sql = format!(
            "MATCH (src:Entity {{uuid: '{}'}}), (dst:Entity {{uuid: '{}'}}) \
             CREATE (src)-[:RELATES_TO {{uuid: '{}', name: '{}', group_id: '{}', \
             fact: '{}', valid_at: {valid_at}, invalid_at: {invalid_at}, \
             attributes: '{}'}}]->(dst)",
            escape(&edge.source_node_uuid),
            escape(&edge.target_node_uuid),
            escape(&edge.uuid),
            escape(&edge.name),
            escape(&edge.group_id),
            escape(&edge.fact),
            escape(&edge.attributes),
        );
        // Direct Entity→Entity rel is non-fatal: Python-schema DBs have no Entity→Entity
        // FROM-TO pair in RELATES_TO, so this insert will fail there. The two-hop links
        // below are sufficient for all reads; the direct rel is kept for schema compatibility
        // with Rust-initialized DBs only.
        if let Err(e) = self.raw_query(&rel_sql) {
            eprintln!(
                "liminis-graph: direct RELATES_TO rel insert failed (non-fatal, Python-schema DB?): {e}"
            );
        }

        // Create both two-hop links in a single statement so either both exist or neither does.
        // Reads use Entity→RelatesToNode_→Entity; the hops carry no meaningful properties —
        // all edge data lives on the RelatesToNode_ shadow node.
        let hops_sql = format!(
            "MATCH (src:Entity {{uuid: '{}'}}), \
                   (rn:RelatesToNode_ {{uuid: '{}'}}), \
                   (dst:Entity {{uuid: '{}'}}) \
             CREATE (src)-[:RELATES_TO]->(rn), (rn)-[:RELATES_TO]->(dst)",
            escape(&edge.source_node_uuid),
            escape(&edge.uuid),
            escape(&edge.target_node_uuid),
        );
        self.raw_query(&hops_sql)
    }

    pub fn insert_mentions_edge(&self, e: &MentionsEdge) -> Result<(), Error> {
        let sql = format!(
            "MATCH (ep:Episodic {{uuid: '{}'}}), (en:Entity {{uuid: '{}'}}) \
             CREATE (ep)-[:MENTIONS {{group_id: '{}'}}]->(en)",
            escape(&e.episodic_uuid),
            escape(&e.entity_uuid),
            escape(&e.group_id),
        );
        self.raw_query(&sql)
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
        let sql = format!(
            "MATCH (ep:Episodic) WHERE ep.group_id = '{}' \
             RETURN ep.uuid, ep.name, ep.group_id, ep.created_at, ep.source, \
             ep.source_description, ep.content, ep.valid_at, ep.entity_edges \
             ORDER BY ep.created_at DESC LIMIT {}",
            escape(group_id),
            last_n,
        );
        let result = self.inner.query(&sql)?;
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
        let sql = format!(
            "MATCH (ep:Episodic {{uuid: '{}'}}) DETACH DELETE ep",
            escape(episode_uuid),
        );
        self.raw_query(&sql)
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
        let escaped_src = escape(source_file);
        let group_clause = match group_ids {
            Some(ids) if !ids.is_empty() => {
                format!(" AND ep.group_id IN {}", format_str_list(ids))
            }
            _ => String::new(),
        };
        let prefix = format!("{}:", source_file);
        let escaped_prefix = escape(&prefix);
        let match_sql = format!(
            "MATCH (ep:Episodic) WHERE (ep.source_description = '{}' \
             OR ep.source_description STARTS WITH '{}'){} RETURN ep.uuid",
            escaped_src, escaped_prefix, group_clause,
        );
        let result = self.inner.query(&match_sql)?;
        let uuids: Vec<String> = result.map(|row| value_as_string(&row[0])).collect();
        if !uuids.is_empty() {
            let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
            let uuid_list = format_str_list(&uuid_refs);
            let delete_sql =
                format!("MATCH (ep:Episodic) WHERE ep.uuid IN {uuid_list} DETACH DELETE ep");
            self.raw_query(&delete_sql)?;
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
            Some(ids) if !ids.is_empty() => {
                format!(" AND ep.group_id IN {}", format_str_list(ids))
            }
            _ => String::new(),
        };
        let match_sql = format!(
            "MATCH (ep:Episodic) WHERE ep.name = '{}'{} RETURN ep.uuid",
            escape(chunk_id),
            group_clause,
        );
        let result = self.inner.query(&match_sql)?;
        let uuids: Vec<String> = result.map(|row| value_as_string(&row[0])).collect();
        if !uuids.is_empty() {
            let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
            let uuid_list = format_str_list(&uuid_refs);
            let delete_sql =
                format!("MATCH (ep:Episodic) WHERE ep.uuid IN {uuid_list} DETACH DELETE ep");
            self.raw_query(&delete_sql)?;
        }
        Ok(uuids)
    }

    /// Returns all Entity nodes in the given group_ids.
    pub fn get_entities_by_group_ids(&self, group_ids: &[&str]) -> Result<Vec<EntityRow>, Error> {
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "MATCH (e:Entity) WHERE e.group_id IN {gid_list} \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes"
        );
        let result = self.inner.query(&sql)?;
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
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.group_id IN {gid_list} \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes"
        );
        self.collect_relates_to_edges(&sql)
    }

    /// Returns RELATES_TO edges for the given UUIDs.
    pub fn get_edges_by_uuids(&self, uuids: &[&str]) -> Result<Vec<RelatesToEdge>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        let uuid_list = format_str_list(uuids);
        let sql = format!(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.uuid IN {uuid_list} \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes"
        );
        self.collect_relates_to_edges(&sql)
    }

    fn collect_relates_to_edges(&self, sql: &str) -> Result<Vec<RelatesToEdge>, Error> {
        let result = self.inner.query(sql)?;
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
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "CALL QUERY_FTS_INDEX('Entity', 'node_name_and_summary', '{}') \
             WITH node, score WHERE node.group_id IN {gid_list} \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT {limit}",
            escape_fts(query),
        );
        self.collect_uuid_score_pairs(&sql)
    }

    /// BM25 full-text search on RelatesToNode_ (facts); returns (uuid, score) pairs.
    pub fn fts_search_edges(
        &self,
        query: &str,
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "CALL QUERY_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', '{}') \
             WITH node, score WHERE node.group_id IN {gid_list} \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT {limit}",
            escape_fts(query),
        );
        self.collect_uuid_score_pairs(&sql)
    }

    /// HNSW vector search on Entity nodes; returns (uuid, distance) pairs (lower = closer).
    pub fn vector_search_entities(
        &self,
        embedding: &[f32],
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let vec_lit = format_float_array(embedding);
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "CALL QUERY_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', {vec_lit}, {limit}) \
             WITH node, distance WHERE node.group_id IN {gid_list} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT {limit}"
        );
        self.collect_uuid_score_pairs(&sql)
    }

    /// HNSW vector search on RelatesToNode_ (facts); returns (uuid, distance) pairs.
    pub fn vector_search_edges(
        &self,
        embedding: &[f32],
        group_ids: &[&str],
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let vec_lit = format_float_array(embedding);
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "CALL QUERY_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx', \
             {vec_lit}, {limit}) \
             WITH node, distance WHERE node.group_id IN {gid_list} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT {limit}"
        );
        self.collect_uuid_score_pairs(&sql)
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
        let vec_lit = format_float_array(embedding);
        let gid_filter = match group_ids {
            Some(gids) if !gids.is_empty() => {
                format!("WHERE node.group_id IN {}", format_str_list(gids))
            }
            _ => String::new(),
        };
        let sql = format!(
            "CALL QUERY_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', {vec_lit}, {limit}) \
             WITH node, distance {gid_filter} \
             RETURN node.uuid, node.name, node.content, node.source_description, \
             node.group_id, node.created_at, node.valid_at, distance \
             ORDER BY distance ASC LIMIT {limit}"
        );
        let result = self.inner.query(&sql)?;
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
        let sql = match group_ids {
            Some(gids) if !gids.is_empty() => {
                let gid_list = format_str_list(gids);
                format!(
                    "MATCH (e:Entity) WHERE e.group_id IN {gid_list} \
                     RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                     e.summary, e.attributes ORDER BY e.uuid DESC LIMIT {limit}"
                )
            }
            _ => format!(
                "MATCH (e:Entity) \
                 RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes ORDER BY e.uuid DESC LIMIT {limit}"
            ),
        };
        let result = self.inner.query(&sql)?;
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
        let sql = match group_ids {
            Some(gids) if !gids.is_empty() => {
                let gid_list = format_str_list(gids);
                format!(
                    "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                     WHERE rn.group_id IN {gid_list} \
                     RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                     rn.valid_at, rn.invalid_at, rn.attributes ORDER BY rn.uuid DESC LIMIT {limit}"
                )
            }
            _ => format!(
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                 rn.valid_at, rn.invalid_at, rn.attributes ORDER BY rn.uuid DESC LIMIT {limit}"
            ),
        };
        self.collect_relates_to_edges(&sql)
    }

    /// Returns 1-hop neighbors via two directed queries (outgoing + incoming), merged in Rust.
    /// Returns `(edges, unique_neighbor_uuids)` truncated to `num_results` edges.
    pub fn get_entity_neighbors(
        &self,
        entity_uuid: &str,
        group_ids: Option<&[&str]>,
        num_results: usize,
    ) -> Result<(Vec<RelatesToEdge>, Vec<String>), Error> {
        let uuid_esc = escape(entity_uuid);
        let gid_filter = match group_ids {
            Some(gids) if !gids.is_empty() => {
                format!("WHERE rn.group_id IN {}", format_str_list(gids))
            }
            _ => String::new(),
        };

        let out_sql = format!(
            "MATCH (c:Entity {{uuid: '{uuid_esc}'}})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(n:Entity) \
             {gid_filter} \
             RETURN rn.uuid, rn.name, c.uuid, n.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes ORDER BY rn.uuid DESC LIMIT {num_results}"
        );
        let in_sql = format!(
            "MATCH (n:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(c:Entity {{uuid: '{uuid_esc}'}}) \
             {gid_filter} \
             RETURN rn.uuid, rn.name, n.uuid, c.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes ORDER BY rn.uuid DESC LIMIT {num_results}"
        );

        let mut edges = self.collect_relates_to_edges(&out_sql)?;
        edges.extend(self.collect_relates_to_edges(&in_sql)?);
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
        let src_esc = escape(source);
        let sql = match group_ids {
            Some(gids) if !gids.is_empty() => {
                let gid_list = format_str_list(gids);
                format!(
                    "MATCH (ep:Episodic)-[:MENTIONS]->(e:Entity) \
                     WHERE ep.source_description CONTAINS '{src_esc}' AND e.group_id IN {gid_list} \
                     RETURN DISTINCT e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                     e.summary, e.attributes LIMIT {limit}"
                )
            }
            _ => format!(
                "MATCH (ep:Episodic)-[:MENTIONS]->(e:Entity) \
                 WHERE ep.source_description CONTAINS '{src_esc}' \
                 RETURN DISTINCT e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes LIMIT {limit}"
            ),
        };
        let result = self.inner.query(&sql)?;
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

    fn collect_uuid_score_pairs(&self, sql: &str) -> Result<Vec<(String, f64)>, Error> {
        let result = self.inner.query(sql)?;
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
        let sql = format!(
            "MATCH (e:Entity) WHERE e.group_id = '{}' \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.name_embedding, e.summary, e.attributes",
            escape(group_id),
        );
        let result = self.inner.query(&sql)?;
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
        let sql = format!(
            "MATCH (e:Entity) WHERE e.group_id = '{}' RETURN count(e)",
            escape(group_id),
        );
        let mut result = self.inner.query(&sql)?;
        if let Some(row) = result.next() {
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
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        let uuid_list = format_str_list(&uuid_refs);
        let sql =
            format!("MATCH (e:Entity) WHERE e.uuid IN {uuid_list} RETURN e.uuid, e.name_embedding");
        let result = self.inner.query(&sql)?;
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
    /// Note: the `ef` search parameter is not configurable in lbug 0.16.1; the lbug default is used.
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
        let sql = format!(
            "MATCH (e:Entity) WHERE e.name = '{}' AND e.group_id = '{}' \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes LIMIT 1",
            escape(name),
            escape(group_id),
        );
        let mut result = self.inner.query(&sql)?;
        if let Some(row) = result.next() {
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
        let sql = format!(
            "MATCH (e:Entity {{uuid: '{}'}}) \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.name_embedding, e.summary, e.attributes",
            escape(uuid),
        );
        let mut result = self.inner.query(&sql)?;
        if let Some(row) = result.next() {
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
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        let uuid_list = format_str_list(&uuid_refs);
        let sql = format!(
            "MATCH (e:Entity) WHERE e.uuid IN {uuid_list} \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes"
        );
        let result = self.inner.query(&sql)?;
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
        let uuid_list = format_str_list(entity_uuids);
        let gid_clause = match group_ids {
            Some(gids) if !gids.is_empty() => {
                format!(" AND ep.group_id IN {}", format_str_list(gids))
            }
            _ => String::new(),
        };
        let sql = format!(
            "MATCH (ep:Episodic)-[:MENTIONS]->(n:Entity) \
             WHERE n.uuid IN {uuid_list}{gid_clause} \
             RETURN n.uuid, ep.uuid, ep.source_description"
        );
        let result = self.inner.query(&sql)?;
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
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        let uuid_list = format_str_list(&uuid_refs);
        // Resolve src/dst via the two-hop links (Entity→RelatesToNode_→Entity).
        let sql = format!(
            "MATCH (rn:RelatesToNode_) WHERE rn.uuid IN {uuid_list} \
             OPTIONAL MATCH (src:Entity)-[:RELATES_TO]->(rn) \
             OPTIONAL MATCH (rn)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, coalesce(src.uuid, ''), coalesce(dst.uuid, ''), \
             rn.group_id, rn.fact, rn.valid_at, rn.invalid_at, rn.attributes"
        );
        let result = self.inner.query(&sql)?;
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
        let uuid_esc = escape(entity_uuid);
        // Outgoing edges (entity is source)
        let out_sql = format!(
            "MATCH (src:Entity {{uuid: '{uuid_esc}'}})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.fact_embedding, rn.created_at"
        );
        // Incoming edges (entity is target)
        let in_sql = format!(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity {{uuid: '{uuid_esc}'}}) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes, rn.fact_embedding, rn.created_at"
        );
        let mut edges = self.collect_full_relates_to_edges(&out_sql)?;
        edges.extend(self.collect_full_relates_to_edges(&in_sql)?);
        Ok(edges)
    }

    fn collect_full_relates_to_edges(&self, sql: &str) -> Result<Vec<RelatesToEdge>, Error> {
        let result = self.inner.query(sql)?;
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
        let sql = format!(
            "MATCH (src:Entity {{uuid: '{}'}})-[:RELATES_TO]->(rn:RelatesToNode_ {{name: '{}'}})-[:RELATES_TO]->(dst:Entity {{uuid: '{}'}}) \
             RETURN count(rn)",
            escape(source_uuid),
            escape(name),
            escape(target_uuid),
        );
        let mut result = self.inner.query(&sql)?;
        if let Some(row) = result.next() {
            Ok(value_as_usize(&row[0]) > 0)
        } else {
            Ok(false)
        }
    }

    /// Returns a full RelatesToEdge by UUID, joining via the RelatesToNode_ shadow node.
    pub fn get_edge_by_uuid(&self, uuid: &str) -> Result<Option<RelatesToEdge>, Error> {
        let sql = format!(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             WHERE rn.uuid = '{}' \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes",
            escape(uuid),
        );
        let mut rows = self.collect_relates_to_edges(&sql)?;
        Ok(rows.pop())
    }

    /// Returns all RELATES_TO edges where the entity with `entity_uuid` is either source or target.
    pub fn get_edges_for_entity(&self, entity_uuid: &str) -> Result<Vec<RelatesToEdge>, Error> {
        let uuid_esc = escape(entity_uuid);
        let out_sql = format!(
            "MATCH (src:Entity {{uuid: '{uuid_esc}'}})-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes"
        );
        let in_sql = format!(
            "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity {{uuid: '{uuid_esc}'}}) \
             RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
             rn.valid_at, rn.invalid_at, rn.attributes"
        );
        let mut edges = self.collect_relates_to_edges(&out_sql)?;
        edges.extend(self.collect_relates_to_edges(&in_sql)?);
        Ok(edges)
    }

    /// Updates the labels array on the Entity with the given UUID.
    pub fn update_entity_labels(&self, uuid: &str, labels: &[String]) -> Result<(), Error> {
        let labels_lit = format_str_array(labels);
        let sql = format!(
            "MATCH (e:Entity {{uuid: '{}'}}) SET e.labels = {}",
            escape(uuid),
            labels_lit,
        );
        self.raw_query(&sql)
    }

    /// Marks the edge identified by `edge_uuid` as invalid by setting `invalid_at`
    /// on the RelatesToNode_ shadow node. Also attempts to set `invalid_at` on the
    /// RELATES_TO relationship property (lbug 0.16.1 may not support SET on rels;
    /// if it fails the error is logged but not propagated).
    pub fn invalidate_edge(&self, edge_uuid: &str, invalid_at: &str) -> Result<(), Error> {
        let uuid_esc = escape(edge_uuid);
        let ts_esc = escape(invalid_at);
        let node_sql = format!(
            "MATCH (rn:RelatesToNode_ {{uuid: '{uuid_esc}'}}) \
             SET rn.invalid_at = timestamp('{ts_esc}')"
        );
        self.raw_query(&node_sql)?;
        // Attempt SET on the RELATES_TO rel — non-fatal if unsupported.
        let rel_sql = format!(
            "MATCH (src:Entity)-[r:RELATES_TO {{uuid: '{uuid_esc}'}}]->(dst:Entity) \
             SET r.invalid_at = timestamp('{ts_esc}')"
        );
        if let Err(e) = self.raw_query(&rel_sql) {
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
        let sql = format!(
            "MATCH (e:Entity) WHERE e.group_id = '{}' AND size(e.labels) = 1 AND 'Entity' IN e.labels \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.summary, e.attributes ORDER BY e.uuid SKIP {} LIMIT {}",
            escape(group_id),
            offset,
            limit,
        );
        let result = self.inner.query(&sql)?;
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
    ///
    /// NOTE: `name_prefix` is single-quote–escaped but not parameterised; use only
    /// trusted input until lbug exposes a parameterised-query API.
    pub fn search_entities(&self, name_prefix: &str) -> Result<Vec<EntityRow>, Error> {
        let sql = format!(
            "MATCH (e:Entity) WHERE e.name STARTS WITH '{}' \
             RETURN e.uuid, e.name, e.group_id, e.summary, e.attributes",
            escape(name_prefix),
        );
        let result = self.inner.query(&sql)?;
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn escape(s: &str) -> String {
    // Cypher single-quoted string literals use backslash escaping, not SQL-style doubling.
    // Backslashes must be escaped first so the newly introduced backslashes are not re-escaped.
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Public escape function for use by other modules (e.g. episode.rs).
pub fn escape_pub(s: &str) -> String {
    escape(s)
}

/// Escapes FTS query strings for embedding in Cypher CALL statements.
fn escape_fts(s: &str) -> String {
    // The query argument is embedded inside a Cypher single-quoted string literal
    // (CALL QUERY_FTS_INDEX(..., '...')), so it requires the same Cypher-compliant
    // backslash escaping as escape(). SQL-style '' doubling causes Cypher parser failure.
    escape(s)
}

pub(crate) fn format_float_array(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:.6}")).collect();
    format!("[{}]", parts.join(","))
}

fn format_str_array(v: &[String]) -> String {
    let parts: Vec<String> = v.iter().map(|s| format!("'{}'", escape(s))).collect();
    format!("[{}]", parts.join(","))
}

pub(crate) fn format_str_list(v: &[&str]) -> String {
    if v.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = v.iter().map(|s| format!("'{}'", escape(s))).collect();
    format!("[{}]", parts.join(", "))
}

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
