use std::path::Path;

use crate::{
    error::Error,
    types::{EntityRow, EpisodicRow, MentionsEdge, RelatesToEdge},
};

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
        db_path:       &str,
        wal_dir:       &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let db_exists = Path::new(db_path).exists();
        let wal_dir_path = Path::new(wal_dir);

        let has_wal = wal_dir_path.exists()
            && wal_dir_path
                .read_dir()
                .map(|rd| {
                    rd.filter_map(|e| e.ok()).any(|e| {
                        e.path().extension().and_then(|x| x.to_str()) == Some("jsonl")
                    })
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
        self.raw_query(&rel_sql)
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
            "CALL CREATE_VECTOR_INDEX('RelatesToNode_', 'relates_to_fact_embedding_idx', \
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

    /// Returns all Entity nodes in the given group_ids.
    pub fn get_entities_by_group_ids(
        &self,
        group_ids: &[&str],
    ) -> Result<Vec<EntityRow>, Error> {
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
    pub fn get_edges_by_group_ids(
        &self,
        group_ids: &[&str],
    ) -> Result<Vec<RelatesToEdge>, Error> {
        let gid_list = format_str_list(group_ids);
        let sql = format!(
            "MATCH (src:Entity)-[r:RELATES_TO]->(dst:Entity) \
             WHERE r.group_id IN {gid_list} \
             RETURN r.uuid, r.name, src.uuid, dst.uuid, r.group_id, r.fact, \
             r.valid_at, r.invalid_at, r.attributes"
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
            "MATCH (src:Entity)-[r:RELATES_TO]->(dst:Entity) \
             WHERE r.uuid IN {uuid_list} \
             RETURN r.uuid, r.name, src.uuid, dst.uuid, r.group_id, r.fact, \
             r.valid_at, r.invalid_at, r.attributes"
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
            "CALL QUERY_FTS_INDEX('Entity', 'entity_name_fts', '{}') \
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
            "CALL QUERY_FTS_INDEX('RelatesToNode_', 'relates_to_fact_fts', '{}') \
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
            "CALL QUERY_VECTOR_INDEX('RelatesToNode_', 'relates_to_fact_embedding_idx', \
             {vec_lit}, {limit}) \
             WITH node, distance WHERE node.group_id IN {gid_list} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT {limit}"
        );
        self.collect_uuid_score_pairs(&sql)
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
                let is_better = best.as_ref().is_none_or(|(s, r)| {
                    sim > *s || (sim == *s && candidate_uuid < r.uuid)
                });
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
        let sql = format!(
            "MATCH (e:Entity) WHERE e.uuid IN {uuid_list} RETURN e.uuid, e.name_embedding"
        );
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
        let bm25_candidates =
            self.fts_search_entities(entity_name, &[group_id], CANDIDATE_K)?;
        let fused_uuids = crate::search::rrf_fuse(&bm25_candidates, &vector_candidates);

        let candidate_embeddings = self.get_entity_embeddings_by_uuids(&fused_uuids)?;

        let mut best: Option<(f32, String)> = None;
        for (uuid, emb) in candidate_embeddings {
            let sim = cosine_similarity(name_embedding, &emb);
            if sim >= threshold {
                let is_better = best.as_ref().is_none_or(|(s, best_uuid)| {
                    sim > *s || (sim == *s && &uuid < best_uuid)
                });
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

    /// Fetches full RelatesToEdge rows for a slice of UUIDs from RelatesToNode_.
    pub fn get_relates_to_by_uuids(
        &self,
        uuids: &[String],
    ) -> Result<Vec<RelatesToEdge>, Error> {
        if uuids.is_empty() {
            return Ok(vec![]);
        }
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        let uuid_list = format_str_list(&uuid_refs);
        // Join RelatesToNode_ with RELATES_TO rel to get source/target node UUIDs
        let sql = format!(
            "MATCH (rn:RelatesToNode_) WHERE rn.uuid IN {uuid_list} \
             OPTIONAL MATCH (src:Entity)-[r:RELATES_TO]->(dst:Entity) WHERE r.uuid = rn.uuid \
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
    s.replace('\'', "''")
}

/// Public escape function for use by other modules (e.g. episode.rs).
pub fn escape_pub(s: &str) -> String {
    escape(s)
}

/// Escapes special FTS query characters (Lucene-style).
fn escape_fts(s: &str) -> String {
    // Only escape single-quotes for the outer SQL string; inner FTS special chars
    // (AND, OR, NOT) are handled by the FTS engine.
    s.replace('\'', "''")
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
    // Format as "YYYY-MM-DD HH:MM:SS" (matches Python graphiti wire format)
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
