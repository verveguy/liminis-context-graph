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
         episodes STRING[], \
         expired_at TIMESTAMP, \
         valid_at TIMESTAMP, \
         invalid_at TIMESTAMP, \
         attributes STRING, \
         relation_type STRING\
         )"
    ))?;
    // Stub tables for graphiti's community/saga subsystem (not implemented in liminis-graph;
    // see #145). They carry no read/write paths, but must EXIST so legacy WAL statements that
    // reference them — notably the bulk edge-delete `MATCH (n)-[e:MENTIONS|RELATES_TO|HAS_MEMBER]
    // ->(m) WHERE e.uuid IN $uuids DELETE e` — bind and execute (a missing table makes the whole
    // multi-type pattern fail to prepare, silently skipping the MENTIONS/RELATES_TO deletes too).
    // Column sets match graphiti's kuzu_driver.py.
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS Community (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         created_at TIMESTAMP, \
         name_embedding FLOAT[{dim}], \
         summary STRING\
         )"
    ))?;
    conn.raw_query(
        "CREATE NODE TABLE IF NOT EXISTS Saga (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         created_at TIMESTAMP\
         )",
    )?;
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
    // graphiti's Kuzu schema declares `uuid STRING PRIMARY KEY` on MENTIONS, but the Rust
    // native write path (`insert_mentions_edge`) does not populate uuid, so a PK would reject
    // those inserts. Use a non-PK `uuid` column (as RELATES_TO already does) — enough for the
    // WAL's MENTIONS MERGE to bind, without breaking native writes.
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS MENTIONS (\
         FROM Episodic TO Entity, \
         uuid STRING, \
         group_id STRING, \
         created_at TIMESTAMP\
         )",
    )?;
    // Stub rel tables for graphiti's community/saga subsystem (see #145). Created so multi-type
    // patterns referencing them bind/execute; no read/write paths in liminis-graph yet.
    // Column sets match graphiti's kuzu_driver.py.
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS HAS_MEMBER (\
         FROM Community TO Entity, \
         FROM Community TO Community, \
         uuid STRING, \
         group_id STRING, \
         created_at TIMESTAMP\
         )",
    )?;
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS HAS_EPISODE (\
         FROM Saga TO Episodic, \
         uuid STRING, \
         group_id STRING, \
         created_at TIMESTAMP\
         )",
    )?;
    conn.raw_query(
        "CREATE REL TABLE IF NOT EXISTS NEXT_EPISODE (\
         FROM Episodic TO Episodic, \
         uuid STRING, \
         group_id STRING, \
         created_at TIMESTAMP\
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
    // Each column is probed independently — no early return — so that a DB which already has
    // relation_type (from the first migration) still gets episodes probed and added if absent.
    // lbug fails at bind time if the column is not in the schema; success means it's present.
    if conn
        .raw_query(
            "MATCH (n:RelatesToNode_) WHERE n.uuid = '_probe_' RETURN n.relation_type LIMIT 0",
        )
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE RelatesToNode_ ADD relation_type STRING") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE RelatesToNode_ ADD relation_type STRING: {e} (non-fatal)");
        }
    }
    if conn
        .raw_query("MATCH (n:RelatesToNode_) WHERE n.uuid = '_probe_' RETURN n.episodes LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE RelatesToNode_ ADD episodes STRING[]") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE RelatesToNode_ ADD episodes STRING[]: {e} (non-fatal)");
        }
    }
    if conn
        .raw_query("MATCH (n:RelatesToNode_) WHERE n.uuid = '_probe_' RETURN n.expired_at LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE RelatesToNode_ ADD expired_at TIMESTAMP") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE RelatesToNode_ ADD expired_at TIMESTAMP: {e} (non-fatal)");
        }
    }
    // MENTIONS rel table gained uuid + created_at to match graphiti's Kuzu schema. The WAL's
    // MENTIONS MERGE sets r.uuid/r.created_at; without these columns replay fails at bind time
    // with `Cannot find property uuid for r`. Probe each column on a MENTIONS rel; ALTER if absent.
    // Anchor the probe on Episodic.uuid (PK index) so it's an O(1) lookup that binds nothing,
    // rather than `WHERE r.group_id = …` which can full-scan the MENTIONS rel table. The RETURN
    // still triggers a binder error if the column is absent, which is what drives the ALTER.
    if conn
        .raw_query("MATCH (n:Episodic {uuid: '_probe_'})-[r:MENTIONS]->() RETURN r.uuid LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE MENTIONS ADD uuid STRING") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE MENTIONS ADD uuid STRING: {e} (non-fatal)");
        }
    }
    if conn
        .raw_query(
            "MATCH (n:Episodic {uuid: '_probe_'})-[r:MENTIONS]->() RETURN r.created_at LIMIT 0",
        )
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE MENTIONS ADD created_at TIMESTAMP") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE MENTIONS ADD created_at TIMESTAMP: {e} (non-fatal)");
        }
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

/// Drops the 3 FTS indexes. Idempotent — errors are suppressed so this is safe to call
/// even when the indexes are already absent (e.g. repeated reload or interrupted reload).
/// Used by `handle_rebuild_from_wal` to enable bulk-load replay without inline FTS maintenance.
pub fn drop_fts_indexes(conn: &Conn<'_>) {
    let _ = conn.raw_query("CALL DROP_FTS_INDEX('Entity', 'node_name_and_summary')");
    let _ = conn.raw_query("CALL DROP_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact')");
    let _ = conn.raw_query("CALL DROP_FTS_INDEX('Episodic', 'episode_content')");
}
