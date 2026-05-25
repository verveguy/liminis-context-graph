use crate::{db::Conn, error::Error};

/// Initialises the full database schema: Entity, Episodic, and edge tables.
///
/// `embedding_dim` controls the `FLOAT[N]` column width — use `768` for bge-base-en-v1.5.
pub fn init(conn: &Conn<'_>, embedding_dim: usize) -> Result<(), Error> {
    if embedding_dim == 0 {
        return Err(Error::QueryFailed("embedding_dim must be > 0".to_string()));
    }
    create_node_tables(conn, embedding_dim)?;
    create_edge_tables(conn, embedding_dim)?;
    create_fts_indexes(conn)?;
    Ok(())
}

fn create_node_tables(conn: &Conn<'_>, dim: usize) -> Result<(), Error> {
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS Entity (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         labels STRING[], \
         created_at TIMESTAMP, \
         name_embedding FLOAT[{dim}], \
         summary STRING, \
         attributes STRING\
         )"
    ))?;
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS Episodic (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         created_at TIMESTAMP, \
         source STRING, \
         source_description STRING, \
         content STRING, \
         content_embedding FLOAT[{dim}], \
         valid_at TIMESTAMP, \
         entity_edges STRING[]\
         )"
    ))?;
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS RelatesToNode_ (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         created_at TIMESTAMP, \
         fact STRING, \
         fact_embedding FLOAT[{dim}], \
         valid_at TIMESTAMP, \
         invalid_at TIMESTAMP, \
         attributes STRING, \
         relation_type STRING\
         )"
    ))?;
    Ok(())
}

/// Creates the RELATES_TO and MENTIONS relationship tables.
///
/// RELATES_TO declares three FROM-TO pairs:
///   Entity→Entity (Rust write path — carries all property values)
///   Entity→RelatesToNode_ and RelatesToNode_→Entity (two-hop navigation hops — no meaningful
///     data on the rel; in Rust-initialized DBs the shared column schema means these rels have
///     NULL values for uuid/name/etc., but reads always pull those from the RelatesToNode_ node)
/// All reads use the two-hop pattern; the Entity→Entity pair is kept for schema compatibility.
/// Note: `IF NOT EXISTS` is a no-op on Python-populated workspaces (schema already created
/// without the Entity→Entity pair). Old Rust-only databases without two-hop links will return
/// empty results from reads — they should be rebuilt.
pub fn create_edge_tables(conn: &Conn<'_>, _dim: usize) -> Result<(), Error> {
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS RELATES_TO (\
         FROM Entity TO Entity, \
         FROM Entity TO RelatesToNode_, \
         FROM RelatesToNode_ TO Entity, \
         uuid STRING, \
         name STRING, \
         group_id STRING, \
         fact STRING, \
         valid_at TIMESTAMP, \
         invalid_at TIMESTAMP, \
         attributes STRING\
         )",
    )?;
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS MENTIONS (\
         FROM Episodic TO Entity, \
         group_id STRING\
         )",
    )?;
    Ok(())
}

/// Applies additive schema migrations to existing workspaces.
///
/// Skips each migration when the target column already exists — probed by attempting a
/// zero-row property access at the Binder stage. lbug raises a Binder exception when the
/// property is unknown; a successful probe means the column is already present.
/// This avoids a lbug bug where `ALTER TABLE ADD` on an existing column corrupts the hash index.
pub fn migrate(conn: &Conn<'_>) {
    // Probe: lbug fails at bind time if `relation_type` is not in the schema.
    let column_exists = conn
        .raw_query(
            "MATCH (n:RelatesToNode_) WHERE n.uuid = '_probe_' RETURN n.relation_type LIMIT 0",
        )
        .is_ok();
    if column_exists {
        return;
    }
    if let Err(e) = conn.raw_query("ALTER TABLE RelatesToNode_ ADD relation_type STRING") {
        eprintln!("liminis-graph: schema migrate: ALTER TABLE RelatesToNode_ ADD relation_type STRING: {e} (non-fatal)");
    }
}

pub(crate) fn create_fts_indexes(conn: &Conn<'_>) -> Result<(), Error> {
    // Errors mean "already exists" — suppress them for idempotency.
    // Index names and covered columns match the upstream Python graphiti-core service (canonical source).
    let _ = conn
        .raw_query("CALL CREATE_FTS_INDEX('Entity', 'node_name_and_summary', ['name', 'summary'])");
    let _ = conn.raw_query(
        "CALL CREATE_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', ['name', 'fact'])",
    );
    let _ = conn.raw_query(
        "CALL CREATE_FTS_INDEX('Episodic', 'episode_content', \
         ['content', 'source', 'source_description'])",
    );
    Ok(())
}
