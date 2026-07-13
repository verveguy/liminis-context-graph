/// DB→WAL dump / compaction logic for `knowledge_dump_wal`.
///
/// Reads the current DB state via paginated queries and writes it into a fresh WAL directory
/// using a caller-supplied `WalWriter` — NOT the live WAL writer in `AppState`. This is the
/// critical invariant: dump output is always isolated from the service's running WAL.
///
/// Phase 1 (nodes) must complete before Phase 2 (edges) begins so that MATCH clauses in edge
/// WAL lines can resolve their endpoint nodes during replay. See ADR-0028.
use crate::{
    db::{
        format_datetime_rfc3339_subsecond, normalize_ts_str_for_dump, value_as_float_array,
        value_as_str_list, value_as_string, Conn,
    },
    error::Error,
    wal::WalWriter,
};

const PAGE_SIZE: usize = 500;

// ── Cypher templates ─────────────────────────────────────────────────────────
// All optional-timestamp params use `CASE WHEN $x IS NULL THEN NULL ELSE timestamp($x) END`
// so that JSON null binds cleanly without triggering Kuzu's timestamp() parser on an empty string.
// Required timestamps use `timestamp($x)` directly.

const ENTITY_CYPHER: &str = "\
    MERGE (n:Entity {uuid: $uuid}) \
    SET n.name = $name, n.group_id = $group_id, n.labels = $labels, \
    n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
    n.summary = $summary, n.attributes = $attributes";

const EPISODIC_CYPHER: &str = "\
    MERGE (n:Episodic {uuid: $uuid}) \
    SET n.name = $name, n.group_id = $group_id, \
    n.created_at = timestamp($created_at), n.source = $source, \
    n.source_description = $source_description, n.content = $content, \
    n.content_embedding = $content_embedding, \
    n.valid_at = CASE WHEN $valid_at IS NULL THEN NULL ELSE timestamp($valid_at) END, \
    n.entity_edges = $entity_edges";

const RELATO_CYPHER: &str = "\
    MERGE (n:RelatesToNode_ {uuid: $uuid}) \
    SET n.name = $name, n.group_id = $group_id, \
    n.created_at = timestamp($created_at), n.fact = $fact, \
    n.fact_embedding = $fact_embedding, n.episodes = $episodes, \
    n.expired_at = CASE WHEN $expired_at IS NULL THEN NULL ELSE timestamp($expired_at) END, \
    n.valid_at = CASE WHEN $valid_at IS NULL THEN NULL ELSE timestamp($valid_at) END, \
    n.invalid_at = CASE WHEN $invalid_at IS NULL THEN NULL ELSE timestamp($invalid_at) END, \
    n.attributes = $attributes, n.relation_type = $relation_type";

const COMMUNITY_CYPHER: &str = "\
    MERGE (n:Community {uuid: $uuid}) \
    SET n.name = $name, n.group_id = $group_id, \
    n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
    n.summary = $summary";

const SAGA_CYPHER: &str = "\
    MERGE (n:Saga {uuid: $uuid}) \
    SET n.name = $name, n.group_id = $group_id, n.created_at = timestamp($created_at)";

// Phase 2: RELATES_TO writes only structural hops — all properties live on RelatesToNode_ (Phase 1).
const RELATES_TO_CYPHER: &str = "\
    MATCH (src:Entity {uuid: $src_uuid}), (rn:RelatesToNode_ {uuid: $rn_uuid}), \
    (dst:Entity {uuid: $dst_uuid}) \
    MERGE (src)-[:RELATES_TO]->(rn) MERGE (rn)-[:RELATES_TO]->(dst)";

const MENTIONS_CYPHER: &str = "\
    MATCH (ep:Episodic {uuid: $ep_uuid}), (en:Entity {uuid: $en_uuid}) \
    MERGE (ep)-[r:MENTIONS]->(en) \
    SET r.uuid = $uuid, r.group_id = $group_id, \
    r.created_at = CASE WHEN $created_at IS NULL THEN NULL ELSE timestamp($created_at) END";

const HAS_EPISODE_CYPHER: &str = "\
    MATCH (sg:Saga {uuid: $sg_uuid}), (ep:Episodic {uuid: $ep_uuid}) \
    MERGE (sg)-[r:HAS_EPISODE]->(ep) \
    SET r.uuid = $uuid, r.group_id = $group_id, \
    r.created_at = CASE WHEN $created_at IS NULL THEN NULL ELSE timestamp($created_at) END";

const HAS_MEMBER_ENTITY_CYPHER: &str = "\
    MATCH (c:Community {uuid: $c_uuid}), (e:Entity {uuid: $e_uuid}) \
    MERGE (c)-[r:HAS_MEMBER]->(e) \
    SET r.uuid = $uuid, r.group_id = $group_id, \
    r.created_at = CASE WHEN $created_at IS NULL THEN NULL ELSE timestamp($created_at) END";

const HAS_MEMBER_COMMUNITY_CYPHER: &str = "\
    MATCH (c:Community {uuid: $c_uuid}), (m:Community {uuid: $m_uuid}) \
    MERGE (c)-[r:HAS_MEMBER]->(m) \
    SET r.uuid = $uuid, r.group_id = $group_id, \
    r.created_at = CASE WHEN $created_at IS NULL THEN NULL ELSE timestamp($created_at) END";

const NEXT_EPISODE_CYPHER: &str = "\
    MATCH (ep1:Episodic {uuid: $ep1_uuid}), (ep2:Episodic {uuid: $ep2_uuid}) \
    MERGE (ep1)-[r:NEXT_EPISODE]->(ep2) \
    SET r.uuid = $uuid, r.group_id = $group_id, \
    r.created_at = CASE WHEN $created_at IS NULL THEN NULL ELSE timestamp($created_at) END";

// ── Public API ────────────────────────────────────────────────────────────────

pub(crate) struct DumpParams {
    pub group_id: Option<String>,
}

pub(crate) struct DumpResult {
    pub nodes_dumped: usize,
    pub edges_dumped: usize,
}

/// Executes the two-phase DB→WAL dump. Writes all nodes (Phase 1) then all structural edges
/// (Phase 2) into `writer`. Caller must hold the service write lock for the duration.
pub(crate) fn run_dump(
    conn: &Conn<'_>,
    params: &DumpParams,
    writer: &mut WalWriter,
) -> Result<DumpResult, Error> {
    let gid = params.group_id.as_deref();
    let nodes_dumped = dump_nodes_phase(conn, writer, gid)?;
    let edges_dumped = dump_edges_phase(conn, writer, gid)?;
    Ok(DumpResult {
        nodes_dumped,
        edges_dumped,
    })
}

// ── Phase 1: nodes ────────────────────────────────────────────────────────────

fn dump_nodes_phase(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    total += dump_entity_nodes(conn, writer, group_id)?;
    total += dump_episodic_nodes(conn, writer, group_id)?;
    total += dump_relato_nodes(conn, writer, group_id)?;
    total += dump_community_nodes(conn, writer, group_id)?;
    total += dump_saga_nodes(conn, writer, group_id)?;
    Ok(total)
}

fn dump_entity_nodes(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_entities_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [uuid, name, group_id, labels, created_at, name_embedding, summary, attributes]
                    let uuid = value_as_string(&row[0]);
                    let name = value_as_string(&row[1]);
                    let grp = value_as_string(&row[2]);
                    let labels = value_as_str_list(&row[3]);
                    let created_at = dump_ts_str(&row[4]);
                    let embedding = value_as_float_array(&row[5]);
                    let summary = value_as_string(&row[6]);
                    let attributes = value_as_string(&row[7]);
                    let params = serde_json::json!({
                        "uuid": uuid,
                        "name": name,
                        "group_id": grp,
                        "labels": labels,
                        "created_at": created_at,
                        "name_embedding": float_slice_to_json(&embedding),
                        "summary": summary,
                        "attributes": attributes,
                    });
                    w.log_mutation(ENTITY_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_episodic_nodes(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_episodics_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [uuid, name, group_id, created_at, source, source_description,
                    //         content, content_embedding, valid_at, entity_edges]
                    let uuid = value_as_string(&row[0]);
                    let name = value_as_string(&row[1]);
                    let grp = value_as_string(&row[2]);
                    let created_at = dump_ts_str(&row[3]);
                    let source = value_as_string(&row[4]);
                    let source_desc = value_as_string(&row[5]);
                    let content = value_as_string(&row[6]);
                    let embedding = value_as_float_array(&row[7]);
                    let valid_at = dump_opt_ts_json(&row[8]);
                    let entity_edges = value_as_str_list(&row[9]);
                    let params = serde_json::json!({
                        "uuid": uuid,
                        "name": name,
                        "group_id": grp,
                        "created_at": created_at,
                        "source": source,
                        "source_description": source_desc,
                        "content": content,
                        "content_embedding": float_slice_to_json(&embedding),
                        "valid_at": valid_at,
                        "entity_edges": entity_edges,
                    });
                    w.log_mutation(EPISODIC_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_relato_nodes(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_relatos_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [uuid, name, group_id, created_at, fact, fact_embedding, episodes,
                    //         expired_at, valid_at, invalid_at, attributes, relation_type]
                    let uuid = value_as_string(&row[0]);
                    let name = value_as_string(&row[1]);
                    let grp = value_as_string(&row[2]);
                    let created_at = dump_ts_str(&row[3]);
                    let fact = value_as_string(&row[4]);
                    let embedding = value_as_float_array(&row[5]);
                    let episodes = value_as_str_list(&row[6]);
                    let expired_at = dump_opt_ts_json(&row[7]);
                    let valid_at = dump_opt_ts_json(&row[8]);
                    let invalid_at = dump_opt_ts_json(&row[9]);
                    let attributes = value_as_string(&row[10]);
                    let relation_type = value_as_string(&row[11]);
                    let params = serde_json::json!({
                        "uuid": uuid,
                        "name": name,
                        "group_id": grp,
                        "created_at": created_at,
                        "fact": fact,
                        "fact_embedding": float_slice_to_json(&embedding),
                        "episodes": episodes,
                        "expired_at": expired_at,
                        "valid_at": valid_at,
                        "invalid_at": invalid_at,
                        "attributes": attributes,
                        "relation_type": relation_type,
                    });
                    w.log_mutation(RELATO_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_community_nodes(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_community_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [uuid, name, group_id, created_at, name_embedding, summary]
                    let uuid = value_as_string(&row[0]);
                    let name = value_as_string(&row[1]);
                    let grp = value_as_string(&row[2]);
                    let created_at = dump_ts_str(&row[3]);
                    let embedding = value_as_float_array(&row[4]);
                    let summary = value_as_string(&row[5]);
                    let params = serde_json::json!({
                        "uuid": uuid,
                        "name": name,
                        "group_id": grp,
                        "created_at": created_at,
                        "name_embedding": float_slice_to_json(&embedding),
                        "summary": summary,
                    });
                    w.log_mutation(COMMUNITY_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_saga_nodes(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_saga_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [uuid, name, group_id, created_at]
                    let uuid = value_as_string(&row[0]);
                    let name = value_as_string(&row[1]);
                    let grp = value_as_string(&row[2]);
                    let created_at = dump_ts_str(&row[3]);
                    let params = serde_json::json!({
                        "uuid": uuid,
                        "name": name,
                        "group_id": grp,
                        "created_at": created_at,
                    });
                    w.log_mutation(SAGA_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

// ── Phase 2: edges ────────────────────────────────────────────────────────────

fn dump_edges_phase(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    total += dump_relates_to_edges(conn, writer, group_id)?;
    total += dump_mentions_edges(conn, writer, group_id)?;
    total += dump_has_episode_edges(conn, writer, group_id)?;
    total += dump_has_member_entity_edges(conn, writer, group_id)?;
    total += dump_has_member_community_edges(conn, writer, group_id)?;
    total += dump_next_episode_edges(conn, writer, group_id)?;
    Ok(total)
}

fn dump_relates_to_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_relates_to_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [src_uuid, rn_uuid, dst_uuid]
                    let src_uuid = value_as_string(&row[0]);
                    let rn_uuid = value_as_string(&row[1]);
                    let dst_uuid = value_as_string(&row[2]);
                    let params = serde_json::json!({
                        "src_uuid": src_uuid,
                        "rn_uuid": rn_uuid,
                        "dst_uuid": dst_uuid,
                    });
                    w.log_mutation(RELATES_TO_CYPHER, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_mentions_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = conn.dump_mentions_page(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            let mut page_written = 0usize;
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [ep_uuid, en_uuid, r_uuid, r_group_id, r_created_at]
                    let ep_uuid = value_as_string(&row[0]);
                    let en_uuid = value_as_string(&row[1]);
                    let r_uuid = value_as_string(&row[2]);
                    // Skip MENTIONS edges with null uuid (pre-migration edges — can't produce
                    // a valid MERGE key; the edge loses no meaningful data since uuid is internal).
                    if r_uuid.is_empty() {
                        eprintln!(
                            "liminis-context-graph: [DUMP WARN] MENTIONS ep={ep_uuid} en={en_uuid} has null uuid — skipping"
                        );
                        continue;
                    }
                    let r_group_id = value_as_string(&row[3]);
                    let r_created_at = dump_opt_ts_json(&row[4]);
                    let params = serde_json::json!({
                        "ep_uuid": ep_uuid,
                        "en_uuid": en_uuid,
                        "uuid": r_uuid,
                        "group_id": r_group_id,
                        "created_at": r_created_at,
                    });
                    w.log_mutation(MENTIONS_CYPHER, params, "")?;
                    page_written += 1;
                }
                Ok(())
            })?;
            total += page_written;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_simple_edge<F>(
    writer: &mut WalWriter,
    group_id: Option<&str>,
    fetch: F,
    cypher: &str,
    src_key: &str,
    dst_key: &str,
) -> Result<usize, Error>
where
    F: Fn(Option<&str>, usize, usize) -> Result<Vec<Vec<lbug::Value>>, Error>,
{
    let mut total = 0;
    let mut offset = 0;
    loop {
        let rows = fetch(group_id, offset, PAGE_SIZE)?;
        let count = rows.len();
        if count > 0 {
            writer.with_chunk(|w| {
                for row in &rows {
                    // cols: [src_uuid, dst_uuid, r_uuid, r_group_id, r_created_at]
                    let src_uuid = value_as_string(&row[0]);
                    let dst_uuid = value_as_string(&row[1]);
                    let r_uuid = value_as_string(&row[2]);
                    let r_group_id = value_as_string(&row[3]);
                    let r_created_at = dump_opt_ts_json(&row[4]);
                    let params = serde_json::json!({
                        src_key: src_uuid,
                        dst_key: dst_uuid,
                        "uuid": r_uuid,
                        "group_id": r_group_id,
                        "created_at": r_created_at,
                    });
                    w.log_mutation(cypher, params, "")?;
                }
                Ok(())
            })?;
            total += count;
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(total)
}

fn dump_has_episode_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    dump_simple_edge(
        writer,
        group_id,
        |gid, off, lim| conn.dump_has_episode_page(gid, off, lim),
        HAS_EPISODE_CYPHER,
        "sg_uuid",
        "ep_uuid",
    )
}

fn dump_has_member_entity_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    dump_simple_edge(
        writer,
        group_id,
        |gid, off, lim| conn.dump_has_member_entity_page(gid, off, lim),
        HAS_MEMBER_ENTITY_CYPHER,
        "c_uuid",
        "e_uuid",
    )
}

fn dump_has_member_community_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    dump_simple_edge(
        writer,
        group_id,
        |gid, off, lim| conn.dump_has_member_community_page(gid, off, lim),
        HAS_MEMBER_COMMUNITY_CYPHER,
        "c_uuid",
        "m_uuid",
    )
}

fn dump_next_episode_edges(
    conn: &Conn<'_>,
    writer: &mut WalWriter,
    group_id: Option<&str>,
) -> Result<usize, Error> {
    dump_simple_edge(
        writer,
        group_id,
        |gid, off, lim| conn.dump_next_episode_page(gid, off, lim),
        NEXT_EPISODE_CYPHER,
        "ep1_uuid",
        "ep2_uuid",
    )
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Formats an lbug Timestamp value as RFC-3339 with microseconds for WAL serialization.
///
/// WAL params use RFC-3339+µs (e.g. `"2024-06-01T12:00:00.123456Z"`) so that dump→wipe→replay
/// cycles preserve sub-second precision. The replay-time `json_value_for_param` handles
/// RFC-3339 as its primary format. Note: this differs from `value_as_timestamp_str`, which
/// emits space-format (`"YYYY-MM-DD HH:MM:SS"`) for Python IPC compatibility.
fn dump_ts_str(v: &lbug::Value) -> String {
    match v {
        lbug::Value::Timestamp(dt) => format_datetime_rfc3339_subsecond(*dt),
        // Normalize string timestamps (RFC-3339 or space-format read-back) to RFC-3339+µs so
        // that WAL dump output is always in a consistent format regardless of how lbug returned
        // the value. Falls through verbatim if the string is not a recognized timestamp format.
        lbug::Value::String(s) => normalize_ts_str_for_dump(s),
        lbug::Value::Null(_) => String::new(),
        _ => v.to_string(),
    }
}

/// Like `dump_ts_str` but returns JSON null for a DB null value.
fn dump_opt_ts_json(v: &lbug::Value) -> serde_json::Value {
    match v {
        lbug::Value::Null(_) => serde_json::Value::Null,
        other => serde_json::Value::String(dump_ts_str(other)),
    }
}

fn float_slice_to_json(v: &[f32]) -> serde_json::Value {
    serde_json::Value::Array(
        v.iter()
            .map(|f| {
                serde_json::Number::from_f64(*f as f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or_else(|| serde_json::Value::Number(serde_json::Number::from(0)))
            })
            .collect(),
    )
}
