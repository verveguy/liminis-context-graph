use crate::{db::Conn, error::Error};

/// Runs the Entity and Episodic `CREATE NODE TABLE IF NOT EXISTS` DDL.
///
/// `embedding_dim` controls the `FLOAT[N]` column width — use `768` for bge-base-en-v1.5
/// or override for other models (AD-5).
pub fn init(conn: &Conn, embedding_dim: usize) -> Result<(), Error> {
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS Entity (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         labels STRING[], \
         created_at TIMESTAMP, \
         name_embedding FLOAT[{embedding_dim}], \
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
         content_embedding FLOAT[{embedding_dim}], \
         valid_at TIMESTAMP, \
         entity_edges STRING[]\
         )"
    ))?;
    Ok(())
}
