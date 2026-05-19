use crate::{
    error::Error,
    types::{EntityRow, EpisodicRow},
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
        Ok(Self { inner })
    }

    /// Opens a connection and loads vector + FTS extensions (AD-2).
    pub fn connect(&self) -> Result<Conn<'_>, Error> {
        let conn = lbug::Connection::new(&self.inner)?;
        let _ = conn.query("INSTALL vector")?;
        let _ = conn.query("LOAD EXTENSION vector")?;
        let _ = conn.query("INSTALL fts")?;
        let _ = conn.query("LOAD EXTENSION fts")?;
        Ok(Conn { inner: conn })
    }
}

impl<'db> Conn<'db> {
    /// Runs a raw Cypher statement; used internally by schema and Conn methods.
    pub(crate) fn raw_query(&self, sql: &str) -> Result<(), Error> {
        let _ = self.inner.query(sql)?;
        Ok(())
    }

    /// Creates the Entity and Episodic node tables. Call once after connecting.
    pub fn init_schema(&self, embedding_dim: usize) -> Result<(), Error> {
        crate::schema::init(self, embedding_dim)
    }

    pub fn insert_entity(&self, row: &EntityRow) -> Result<(), Error> {
        let sql = format!(
            "CREATE (:Entity {{uuid: '{}', name: '{}', group_id: '{}', labels: {}, \
             created_at: timestamp('{}'), name_embedding: {}, summary: '{}', \
             attributes: '{}'}})",
            escape(&row.uuid),
            escape(&row.name),
            escape(&row.group_id),
            format_str_array(&row.labels),
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

    /// Creates HNSW vector indexes on Entity and Episodic.
    ///
    /// Must be called AFTER all insert_entity / insert_episodic calls — HNSW indexes
    /// block in-place vector writes (AD-4).
    pub fn create_vector_indexes(&self) -> Result<(), Error> {
        self.raw_query(
            "CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', \
             'name_embedding', metric := 'cosine')",
        )?;
        self.raw_query(
            "CALL CREATE_VECTOR_INDEX('Episodic', 'episodic_content_embedding_idx', \
             'content_embedding', metric := 'cosine')",
        )?;
        Ok(())
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

fn format_float_array(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:.6}")).collect();
    format!("[{}]", parts.join(","))
}

fn format_str_array(v: &[String]) -> String {
    let parts: Vec<String> = v.iter().map(|s| format!("'{}'", escape(s))).collect();
    format!("[{}]", parts.join(","))
}

fn value_as_string(v: &lbug::Value) -> String {
    match v {
        lbug::Value::String(s) => s.clone(),
        // Null and non-STRING variants are not expected for STRING schema columns;
        // produce an empty string rather than a Debug representation.
        _ => String::new(),
    }
}
