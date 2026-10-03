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
    // `summary_embedding` is a deliberate divergence from graphiti's kuzu_driver.py schema-parity
    // rule (like `WalPosition.generation`, see ADR-0353/ADR-0387): upstream's Entity table has no
    // summary vector, only `name_embedding`. Without it, meaning-based retrieval against an
    // entity's `summary` was lexical-only (FTS) — a paraphrase sharing no vocabulary with the
    // summary couldn't be found by vector similarity. See ADR-0470 (issue #470).
    // `lookup_key` (issue #221, ADR-0221) is a deliberate divergence from graphiti's
    // kuzu_driver.py schema-parity rule, like `summary_embedding` above: it materializes
    // `group_id + '\x1f' + lower(name)` (computed host-side, see `db::compute_lookup_key`)
    // so `get_entity_by_name_ci` can be answered by an ART-indexed equality lookup instead of
    // an in-process accelerator (ADR-0038's `NameIndex`, which this column and its index
    // replace) or an unindexed `lower(e.name) = $x` scan.
    // `kind` (issue #615, ADR-0615) is a third documented divergence from graphiti's
    // kuzu_driver.py schema, additive like the two above: the single identity-bearing kind of an
    // entity (default `Entity`), part of `lookup_key`'s composition
    // (`group_id ␟ kind ␟ lower(name)`). Graphiti-shaped reads/writes never mention it, so they
    // are unaffected (FR-013).
    // `ingested_at` (issue #673, ADR-0673) is a fourth documented divergence from graphiti's
    // kuzu_driver.py schema, additive like the three above and present on exactly `Entity`,
    // `Episodic` and `RelatesToNode_`: the service-clock time the record was first written
    // ("knowledge time"), kept separately from the caller-supplied event time that feeds
    // `created_at`/`valid_at`. Nullable, so graphiti-shaped statements that never mention it
    // still bind; legacy rows are filled by `ensure_ingested_at_backfill`.
    conn.raw_query(&format!(
        "CREATE NODE TABLE IF NOT EXISTS Entity (\
         uuid STRING PRIMARY KEY, \
         name STRING, \
         group_id STRING, \
         labels STRING[], \
         created_at TIMESTAMP, \
         name_embedding FLOAT[{dim}], \
         summary STRING, \
         attributes STRING, \
         summary_embedding FLOAT[{dim}], \
         lookup_key STRING, \
         kind STRING, \
         ingested_at TIMESTAMP\
         )"
    ))?;
    // `attributes` (issue #528) is a deliberate divergence from graphiti's kuzu_driver.py
    // schema-parity rule, like `Entity.summary_embedding`/`Entity.lookup_key` above: it holds
    // caller-supplied structured metadata (a JSON object serialized to a string, matching
    // `Entity.attributes`/`RelatesToNode_.attributes`) directly on the episode node, co-located
    // with the facts extracted from that episode's prose (reachable via the existing MENTIONS
    // edge to Entity). See ADR-0528.
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
         entity_edges STRING[], \
         attributes STRING, \
         ingested_at TIMESTAMP\
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
         relation_type STRING, \
         ingested_at TIMESTAMP\
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
    // Singleton metadata table recording the highest WAL seq whose mutations are committed
    // in this graph (issue #353). This is a deliberate divergence from graphiti's
    // kuzu_driver.py schema-parity rule — graphiti has no equivalent, since it does not
    // itself track an applied WAL position. See ADR-0353 for the rationale (an O(1) boot
    // check needs a persisted cursor; ADR-0026's episode-cursor mechanism is retroactive but
    // requires a WAL scan, unsuitable for a per-`knowledge_status`-call hot path). A single
    // row with id: 'singleton' holds the current position; row-absence means "unknown".
    // `generation` (issue #387) extends this same divergence rather than introducing a second
    // one: it scopes `applied_seq` to the WAL stream generation it was recorded against, so a
    // stream reset can be detected as "different generation" rather than misread as forward
    // progress. See ADR-0387 for why this lives on the same row instead of a separate table or
    // sidecar file.
    // `embedding_model`/`embedding_dim` (issue #440, FR-007) record the embedder identity under
    // which this group's currently-applied vectors were computed — compared at query/startup
    // time against the running embedder's identity to surface a mismatch (FR-008). This is the
    // sole surviving model-identity mechanism (issue #526, FR-004): the write-time
    // `.wal-embedding-model.json` sidecar this comment used to also reference was removed once
    // replay stopped ever binding a stored vector, since nothing was left for it to govern.
    conn.raw_query(
        "CREATE NODE TABLE IF NOT EXISTS WalPosition (\
         id STRING PRIMARY KEY, \
         applied_seq INT64, \
         generation STRING, \
         embedding_model STRING, \
         embedding_dim INT64\
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
pub fn migrate(conn: &Conn<'_>, dim: usize) {
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
    // WalPosition gained `generation` (issue #387) to scope applied_seq to the stream
    // generation it was recorded against. Probe via the singleton row id (PK index), which
    // exists whenever any group has ever recorded a position; an absent row is not an error
    // here, only a genuine binder failure (missing column) drives the ALTER.
    if conn
        .raw_query("MATCH (n:WalPosition) WHERE n.id = '_probe_' RETURN n.generation LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE WalPosition ADD generation STRING") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE WalPosition ADD generation STRING: {e} (non-fatal)");
        }
    }
    // WalPosition gained `embedding_model`/`embedding_dim` (issue #440, FR-007) to record the
    // embedder identity under which this group's currently-applied vectors were computed.
    if conn
        .raw_query("MATCH (n:WalPosition) WHERE n.id = '_probe_' RETURN n.embedding_model LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE WalPosition ADD embedding_model STRING") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE WalPosition ADD embedding_model STRING: {e} (non-fatal)");
        }
    }
    if conn
        .raw_query("MATCH (n:WalPosition) WHERE n.id = '_probe_' RETURN n.embedding_dim LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE WalPosition ADD embedding_dim INT64") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE WalPosition ADD embedding_dim INT64: {e} (non-fatal)");
        }
    }
    // Entity gained `summary_embedding` (issue #470) so an entity's summary is semantically
    // (not just lexically) searchable. Probe first: a fresh DB already has the column from
    // `create_node_tables`, so the ALTER only runs against pre-existing workspaces. Immediately
    // after adding it, zero-fill every existing row *before* any vector index is built over the
    // column (that happens later, in `build_indices_and_constraints`) — a plain `SET` is only
    // legal on an indexed column before the index exists (see `update_entity_core`'s doc comment
    // in db.rs for the HNSW-rejects-SET-on-indexed-column constraint), so this is the one window
    // where every row can be given a real (all-zero) vector rather than leaving it NULL. This
    // sidesteps needing to know whether `CREATE_VECTOR_INDEX` tolerates NULL entries: after this
    // migration, `summary_embedding` is always a same-length `FLOAT[dim]` vector, never absent.
    // The zero-vector is the same sentinel `handle_assert_entity`/`episode.rs` use for an
    // empty-string summary, so a not-yet-backfilled pre-existing entity is indistinguishable from
    // one created with an empty summary — both simply don't contribute to summary-vector search
    // until a real embedding replaces the zero vector (via `knowledge_backfill_summary_embeddings`).
    if conn
        .raw_query("MATCH (n:Entity) WHERE n.uuid = '_probe_' RETURN n.summary_embedding LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query(&format!(
            "ALTER TABLE Entity ADD summary_embedding FLOAT[{dim}]"
        )) {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE Entity ADD summary_embedding FLOAT[{dim}]: {e} (non-fatal)");
        } else if let Err(e) = zero_fill_null_entity_summary_embeddings(conn, dim) {
            eprintln!("liminis-context-graph: schema migrate: zero-fill Entity.summary_embedding: {e} (non-fatal)");
        }
    }
    // Entity gained `lookup_key` (issue #221) to serve `get_entity_by_name_ci` from a
    // database-native ART index instead of ADR-0038's in-process `NameIndex`. Probe first: a
    // fresh DB already has the column from `create_node_tables`, so the ALTER only runs
    // against pre-existing workspaces. Backfill immediately after, in the same one-shot
    // migration step (FR-005) — `Db::build_indices_and_constraints`'s later
    // `create_entity_lookup_key_index` call builds the ART index over whatever the column
    // holds at that point, so every existing row must have a correct key before that runs.
    //
    // Whether that backfill *succeeded* is persisted in `SchemaState` (below), not just the
    // in-process `LookupKeyStatus` flag: a failed backfill after a successful `ALTER` leaves
    // the column present, so on the next open this probe would otherwise succeed and skip the
    // `if` block entirely — silently never retrying, while `lookup_key_migrated()` resets to
    // its `true` default and `knowledge_status` reports healthy. See `ensure_lookup_key_backfill`
    // below for how the persisted marker closes that gap without reintroducing an O(N) `Entity`
    // scan on the clean (already-migrated) startup path.
    ensure_lookup_key_backfill(conn);
    // Entity/Episodic/RelatesToNode_ gained `ingested_at` (issue #673). Probe first — lbug
    // corrupts its hash index if `ALTER TABLE ADD` runs on an existing column. The value backfill
    // is `ensure_ingested_at_backfill` (it needs the WAL dir, which `migrate` does not have).
    for table in ["Entity", "Episodic", "RelatesToNode_"] {
        if conn
            .raw_query(&format!(
                "MATCH (n:{table}) WHERE n.uuid = '_probe_' RETURN n.ingested_at LIMIT 0"
            ))
            .is_err()
        {
            if let Err(e) =
                conn.raw_query(&format!("ALTER TABLE {table} ADD ingested_at TIMESTAMP"))
            {
                eprintln!("liminis-context-graph: schema migrate: ALTER TABLE {table} ADD ingested_at TIMESTAMP: {e} (non-fatal)");
            }
        }
    }
    // Seed the known-kinds registry (issue #615) from the migrated data, so broad name
    // resolution probes every kind an existing database holds from the first request.
    conn.refresh_known_kinds();
    // Episodic gained `attributes` (issue #528) to hold caller-supplied structured metadata
    // directly on the episode node. Probe first: a fresh DB already has the column from
    // `create_node_tables`, so the ALTER only runs against pre-existing workspaces. Unlike
    // `summary_embedding`, no index is ever built over this column, so the zero-fill below is
    // not forced by an indexing constraint — it's done anyway so a migrated pre-existing episode
    // reads back the same empty-JSON-object string (`"{}"`) as a freshly-created episode that
    // omitted `attributes`, rather than the different (and non-JSON) `""` that `value_as_string`
    // produces for a NULL column. See ADR-0528.
    if conn
        .raw_query("MATCH (n:Episodic) WHERE n.uuid = '_probe_' RETURN n.attributes LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE Episodic ADD attributes STRING") {
            eprintln!("liminis-context-graph: schema migrate: ALTER TABLE Episodic ADD attributes STRING: {e} (non-fatal)");
        } else if let Err(e) = zero_fill_null_episodic_attributes(conn) {
            eprintln!("liminis-context-graph: schema migrate: zero-fill Episodic.attributes: {e} (non-fatal)");
        }
    }
}

/// Zero-fills any `Entity` row whose `summary_embedding` is `NULL` (issue #470). Idempotent — a
/// no-op when no row is `NULL`. `migrate`'s `ALTER` branch only reaches rows already present in
/// the DB at migration time; it does NOT cover a row created afterward by replaying a pre-#470
/// WAL recording verbatim: `WalReplayer` executes raw recorded Cypher, and a `MERGE ... ON CREATE
/// SET` that never mentions `summary_embedding` (because it was logged before that column
/// existed) leaves the column `NULL` on the newly-created row — empirically verified in
/// `handlers_wal_admin.rs`'s `test_rebuild_from_wal_force_clear_zero_fills_legacy_entity_summary_embedding`,
/// since a fixed-size `FLOAT[dim]` ARRAY column does not uniformly default an omitted property to
/// zero across every write path. Callers that rebuild a DB from WAL (`Db::open_or_rebuild`,
/// `handle_rebuild_from_wal`, the `knowledge_recover*` family) must call this after replay and
/// before the first `create_vector_indexes`/`build_indices_and_constraints` call, so
/// `CREATE_VECTOR_INDEX` never has to face a `NULL` entry (untested, unsupported) and the "always
/// a same-length `FLOAT[dim]` vector, never absent" invariant holds regardless of how a row was
/// created.
pub fn zero_fill_null_entity_summary_embeddings(conn: &Conn<'_>, dim: usize) -> Result<(), Error> {
    conn.exec_params(
        "MATCH (n:Entity) WHERE n.summary_embedding IS NULL SET n.summary_embedding = $zero",
        serde_json::json!({ "zero": vec![0.0f32; dim] }),
    )
}

/// Zero-fills any `Episodic` row whose `attributes` is `NULL` (issue #528) to the empty-JSON-
/// object string `"{}"`. Idempotent — a no-op when no row is `NULL`. Mirrors
/// `zero_fill_null_entity_summary_embeddings`'s dual-call-site shape: `migrate`'s `ALTER` branch
/// only reaches rows already present in the DB at migration time; it does NOT cover a row created
/// afterward by replaying a pre-#528 WAL recording verbatim, since `WalReplayer` executes raw
/// recorded Cypher and a `MERGE ... ON CREATE SET` that never mentions `attributes` (because it
/// was logged before that column existed) leaves the column `NULL` on the newly-created row.
/// Callers that rebuild a DB from WAL (`Db::open_or_rebuild`, `handle_rebuild_from_wal`, the
/// `knowledge_recover*` family) must call this after replay, so every episode's `attributes`
/// column is always a parseable JSON string, never the non-JSON `""` a NULL column would
/// otherwise read back as via `value_as_string`.
pub fn zero_fill_null_episodic_attributes(conn: &Conn<'_>) -> Result<(), Error> {
    conn.exec_params(
        "MATCH (n:Episodic) WHERE n.attributes IS NULL SET n.attributes = $empty",
        serde_json::json!({ "empty": "{}" }),
    )
}

/// Backfills `lookup_key` for every `Entity` row where it's `NULL` (issue #221 FR-005/FR-006).
/// Idempotent — a no-op when no row is `NULL`. Computes each row's key in Rust via
/// `db::compute_lookup_key` (never Cypher `lower()`, for the Unicode-consistency reason
/// documented there) and writes it back one row at a time, rather than a single bulk
/// Cypher `SET` — a deliberate, acknowledged-slower trade for guaranteed key consistency
/// with every other writer.
///
/// Called from two places, mirroring `zero_fill_null_entity_summary_embeddings`'s dual-call-site
/// shape: `migrate`'s one-shot ALTER-triggered backfill (existing rows at migration time), and
/// every WAL-rebuild/recovery site (`Db::open_or_rebuild`, `handle_rebuild_from_wal`, the
/// `knowledge_recover*` family) — because `WalReplayer::replay` executes raw recorded Cypher
/// verbatim, a replayed `Entity` CREATE never sets `lookup_key` (`dump.rs`'s `ENTITY_CYPHER`
/// template is deliberately left unchanged, per ADR-0221 — this backfill is the only
/// self-sufficiency mechanism a dump→replay round trip needs). Must run before the caller's own
/// `build_indices_and_constraints`/`create_entity_lookup_key_index` ever builds the ART index
/// over the column, so the index is never built while rows are still `NULL`.
pub fn backfill_entity_lookup_keys(conn: &Conn<'_>) -> Result<(), Error> {
    // Issue #615 widened this from `lookup_key IS NULL`: an entity with a NULL `kind` is one
    // written before kinds existed (an upgraded database's rows, an old WAL's records, the #217
    // corpus), and its key — if it has one at all — is in the old two-field format and must be
    // recomputed under the new composition. Such rows are all kind `Entity` (D3). A row with a
    // kind but no key is the original #221 case.
    let rows = conn.query_params(
        "MATCH (n:Entity) WHERE n.kind IS NULL OR n.lookup_key IS NULL \
         RETURN n.uuid, n.name, n.group_id, n.kind, n.labels",
        serde_json::json!({}),
    )?;
    for row in rows {
        let uuid = crate::db::value_as_string(&row[0]);
        let name = crate::db::value_as_string(&row[1]);
        let group_id = crate::db::value_as_string(&row[2]);
        let kind = crate::db::value_as_kind(&row[3]);
        let labels = crate::db::value_as_str_list(&row[4]);
        let key = crate::db::compute_lookup_key(&group_id, &kind, &name);
        // `kind ∈ labels` (FR-001) is restored here too. The label *set* only changes for a row
        // whose labels were clobbered out-of-band, but `labels_with_kind` also moves `Entity` to
        // the front (enforce_entity_first), so rows written by older lcg versions with `Entity`
        // later in the list (e.g. `[Object, Entity]`) are reordered to `[Entity, Object]`. On a
        // real 0.14.x notebook that was 144 of 198 entities; harmless for set-semantics readers.
        let labels = crate::db::labels_with_kind(&labels, &kind);
        conn.exec_params(
            "MATCH (n:Entity {uuid: $uuid}) SET n.kind = $kind, n.lookup_key = $key, \
             n.labels = $labels",
            serde_json::json!({ "uuid": uuid, "kind": kind, "key": key, "labels": labels }),
        )?;
    }
    conn.refresh_known_kinds();
    Ok(())
}

/// `SchemaState` key recording that the `ingested_at` upgrade backfill (issue #673) has run to
/// completion against this database. See [`ensure_ingested_at_backfill`].
pub(crate) const INGESTED_AT_BACKFILL_STATE_KEY: &str = "ingested_at_backfill_v1";

/// The node tables that carry `ingested_at`, with the label a WAL `CREATE` names them by.
const INGESTED_AT_TABLES: [&str; 3] = ["Entity", "Episodic", "RelatesToNode_"];

/// If `cypher` is a *native creating* statement for one of the [`INGESTED_AT_TABLES`], returns
/// that table. "Native" is deliberate: it is the only shape whose WAL `ts` means "when the
/// service first wrote this record" — `Conn::insert_*`'s bound `CREATE (:Label {…})`, or the
/// legacy graphiti-era `MERGE (n:Label {uuid: $uuid}) ON CREATE SET …`. The dump/compaction
/// templates are `MERGE … SET` and are stamped with the *compaction* time, which says nothing
/// about when anything was learned, so they are never a creating line (records whose only WAL
/// trace is a dump line take the `created_at` fallback).
fn creating_table(cypher: &str) -> Option<&'static str> {
    let c = cypher.trim_start();
    INGESTED_AT_TABLES.into_iter().find(|label| {
        c.starts_with(&format!("CREATE (:{label} {{"))
            || (c.starts_with("MERGE (")
                && c.contains(&format!(":{label} {{uuid: $uuid}})"))
                && c.contains("ON CREATE SET"))
    })
}

/// WAL pass of the `ingested_at` backfill (issue #673, FR-007 signal 1): for every record whose
/// `ingested_at` is still NULL, sets it to the `ts` of the earliest WAL line that natively
/// created it. Streams the WAL files once, in first-`seq` order (so the first creating line
/// wins), and writes one `IS NULL`-guarded point update per creating line — memory stays O(1)
/// and a re-run is a no-op. Lines that already bind `$ingested_at` carry their own value through
/// replay and are skipped. Unrecorded: marker/derived state must not reach the WAL.
fn backfill_ingested_at_from_wal(conn: &Conn<'_>, wal_dir: &std::path::Path) -> Result<(), Error> {
    use std::io::BufRead;
    if !wal_dir.is_dir() {
        return Ok(());
    }
    let mut files: Vec<(Option<u64>, std::path::PathBuf)> = std::fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .map(|p| (crate::wal::first_seq_in_file(&p), p))
        .collect();
    files.sort_by(|(sa, pa), (sb, pb)| match (sa, sb) {
        (Some(a), Some(b)) => a.cmp(b).then_with(|| pa.file_name().cmp(&pb.file_name())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => pa.file_name().cmp(&pb.file_name()),
    });
    for (_, path) in files {
        let reader = std::io::BufReader::new(std::fs::File::open(&path)?);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                // A line that is not valid UTF-8 (a torn write) has already been consumed: skip
                // just that line. Stopping here would still return `Ok`, the caller would record
                // the marker `complete`, and every later creating line in this file would take
                // the approximate `created_at` fallback with no retry.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                // A genuine read failure: surface it so the status is recorded `failed` and the
                // backfill is retried on the next open.
                Err(e) => return Err(e.into()),
            };
            // Cheap pre-filter before paying for a JSON parse of a (possibly vector-bearing) line.
            if !line.contains("\"cypher\"")
                || !(line.contains("CREATE (:") || line.contains("ON CREATE SET"))
            {
                continue;
            }
            let Ok(wl) = serde_json::from_str::<crate::wal::WalLine>(&line) else {
                continue;
            };
            let Some(table) = creating_table(&wl.cypher) else {
                continue;
            };
            if wl.cypher.contains("$ingested_at") {
                continue;
            }
            let Some(uuid) = wl.params.get("uuid").and_then(|u| u.as_str()) else {
                continue;
            };
            // `ts` is `%Y-%m-%dT%H:%M:%S%.6f+00:00`; `ingested_at` is a TIMESTAMP_PARAM_NAMES
            // name, so it binds as a typed Timestamp.
            conn.exec_params_unrecorded(
                &format!(
                    "MATCH (n:{table} {{uuid: $uuid}}) WHERE n.ingested_at IS NULL \
                     SET n.ingested_at = $ingested_at"
                ),
                serde_json::json!({ "uuid": uuid, "ingested_at": wl.ts }),
            )?;
        }
    }
    Ok(())
}

/// Backfills `ingested_at` on every Entity/Episodic/RelatesToNode_ row where it is NULL (issue
/// #673, FR-007), in precedence order: (1) the WAL `ts` of the line that created the record, when
/// `wal_dirs` are given; (2) the record's own `created_at`. Idempotent (every step is guarded by
/// `ingested_at IS NULL`), so rows that already carry an ingest time are never touched.
///
/// Values from (2) are *approximate*: for records written before this feature `created_at` was
/// the caller-supplied event time, not the ingest time. Documented in `docs/ipc-mcp-reference.md`.
pub fn backfill_ingested_at(conn: &Conn<'_>, wal_dirs: &[std::path::PathBuf]) -> Result<(), Error> {
    for dir in wal_dirs {
        backfill_ingested_at_from_wal(conn, dir)?;
    }
    for table in INGESTED_AT_TABLES {
        conn.exec_params_unrecorded(
            &format!(
                "MATCH (n:{table}) WHERE n.ingested_at IS NULL AND n.created_at IS NOT NULL \
                 SET n.ingested_at = n.created_at"
            ),
            serde_json::json!({}),
        )?;
    }
    Ok(())
}

/// Runs [`backfill_ingested_at`] and persists the outcome to `SchemaState`. Unconditional (no
/// marker check): the entry point for every WAL-rebuild/recovery site, because replaying a legacy
/// WAL creates NULL-`ingested_at` rows *after* an earlier `"complete"` marker was written, exactly
/// like `lookup_key` (see [`backfill_entity_lookup_keys_and_record_status`]). Non-fatal.
pub(crate) fn backfill_ingested_at_and_record_status(
    conn: &Conn<'_>,
    wal_dirs: &[std::path::PathBuf],
) {
    if let Err(e) = ensure_schema_state_table(conn) {
        eprintln!(
            "liminis-context-graph: ingested_at backfill: ensure SchemaState table (non-fatal): {e}"
        );
    }
    let status = match backfill_ingested_at(conn, wal_dirs) {
        Ok(()) => "complete",
        Err(e) => {
            eprintln!("liminis-context-graph: backfill ingested_at (non-fatal): {e}");
            "failed"
        }
    };
    if let Err(e) = set_schema_state_status(conn, INGESTED_AT_BACKFILL_STATE_KEY, status) {
        eprintln!(
            "liminis-context-graph: record ingested_at backfill status in SchemaState (non-fatal): {e}"
        );
    }
}

/// Upgrade-path entry point (issue #673): backfills `ingested_at` once for a database that
/// predates the column. A persisted `"complete"` marker makes every later open an O(1) point
/// lookup; no marker, or `"failed"`, runs the backfill. Needs the group's WAL dir for the
/// preferred signal, which `migrate()` does not have — so it is called right after `init_schema`
/// at each open site that does (`Db::open_or_rebuild`, service startup). An empty `wal_dirs` is a
/// no-op rather than a `created_at`-only pass, so the WAL signal is not lost to a site that
/// cannot see it; a later call with a WAL dir still does the full job.
pub fn ensure_ingested_at_backfill(conn: &Conn<'_>, wal_dirs: &[std::path::PathBuf]) {
    if wal_dirs.is_empty() {
        return;
    }
    if let Err(e) = ensure_schema_state_table(conn) {
        eprintln!(
            "liminis-context-graph: ingested_at backfill: ensure SchemaState table (non-fatal): {e}"
        );
    }
    match schema_state_status(conn, INGESTED_AT_BACKFILL_STATE_KEY) {
        Ok(Some(status)) if status == "complete" => {}
        Ok(_) => backfill_ingested_at_and_record_status(conn, wal_dirs),
        Err(e) => eprintln!(
            "liminis-context-graph: read SchemaState for ingested_at backfill (non-fatal): {e}"
        ),
    }
}

/// Key under which the `lookup_key` backfill's completion state is persisted in `SchemaState`
/// (see `ensure_lookup_key_backfill`).
///
/// Versioned (issue #615): the original `entity_lookup_key_backfill` marker only proved every
/// row had *a* key, and old-format keys are non-NULL. Renaming the key forces every database
/// through the widened backfill (`kind IS NULL OR lookup_key IS NULL`) exactly once.
const LOOKUP_KEY_BACKFILL_STATE_KEY: &str = "entity_kind_lookup_key_v2";

/// A minimal, generic migration-state marker table: one row per named migration step, keyed by
/// a stable string identifier. Introduced by issue #221 to close a gap the PR's own human review
/// caught — see `ensure_lookup_key_backfill`'s doc comment for the failure mode this exists to
/// prevent. `CREATE NODE TABLE IF NOT EXISTS` is a catalog check, not a scan, so calling this
/// unconditionally on every `migrate()` run is cheap regardless of database size or age.
fn ensure_schema_state_table(conn: &Conn<'_>) -> Result<(), Error> {
    // Unrecorded: derived/marker state must never reach the WAL (ADR-0015, ADR-0649), and
    // this is now also called from `create_fts_indexes` on handler connections.
    conn.query_unrecorded(
        "CREATE NODE TABLE IF NOT EXISTS SchemaState (key STRING PRIMARY KEY, status STRING)",
    )?;
    Ok(())
}

/// Point lookup (by `SchemaState`'s primary key) for a migration step's persisted status.
/// `Ok(None)` means no marker has ever been written for this key — a genuinely fresh table
/// (nothing has run yet) or a pre-existing database migrated before this marker table existed.
fn schema_state_status(conn: &Conn<'_>, key: &str) -> Result<Option<String>, Error> {
    let rows = conn.query_params(
        "MATCH (s:SchemaState {key: $key}) RETURN s.status",
        serde_json::json!({ "key": key }),
    )?;
    Ok(rows
        .into_iter()
        .next()
        .map(|row| crate::db::value_as_string(&row[0])))
}

fn set_schema_state_status(conn: &Conn<'_>, key: &str, status: &str) -> Result<(), Error> {
    conn.exec_params(
        "MERGE (s:SchemaState {key: $key}) SET s.status = $status",
        serde_json::json!({ "key": key, "status": status }),
    )
}

/// Runs `backfill_entity_lookup_keys` and persists its outcome to `SchemaState`, plus the
/// in-process `LookupKeyStatus` flag (`knowledge_status`'s `name_index_trusted`, FR-012).
fn run_lookup_key_backfill_and_record_status(conn: &Conn<'_>) {
    match backfill_entity_lookup_keys(conn) {
        Ok(()) => {
            if let Err(e) = set_schema_state_status(conn, LOOKUP_KEY_BACKFILL_STATE_KEY, "complete")
            {
                eprintln!(
                    "liminis-context-graph: schema migrate: record lookup_key backfill success in SchemaState (non-fatal): {e}"
                );
                // The backfill itself succeeded, but we couldn't persist that fact — treat
                // this conservatively as untrusted rather than silently reporting healthy.
                conn.mark_lookup_key_migration_failed();
            } else {
                // Reset the in-process flag on a successful (re)backfill — otherwise a `Db`
                // that failed once and was then successfully retried within the same process
                // lifetime would report `name_index_trusted: false` forever, even though
                // `SchemaState` and the data itself are now both correct.
                conn.mark_lookup_key_migration_succeeded();
            }
        }
        Err(e) => {
            eprintln!(
                "liminis-context-graph: schema migrate: backfill Entity.lookup_key (non-fatal): {e}"
            );
            conn.mark_lookup_key_migration_failed();
            if let Err(e2) = set_schema_state_status(conn, LOOKUP_KEY_BACKFILL_STATE_KEY, "failed")
            {
                eprintln!(
                    "liminis-context-graph: schema migrate: record lookup_key backfill failure in SchemaState (non-fatal): {e2}"
                );
            }
        }
    }
}

/// Runs `backfill_entity_lookup_keys` and persists the outcome to both `SchemaState` and the
/// in-process `LookupKeyStatus` flag — the entry point for every `lookup_key` backfill call site
/// outside `migrate()` itself: the WAL-rebuild and recovery paths (`Db::open_or_rebuild`,
/// `handle_rebuild_from_wal`, the `knowledge_recover*` family).
///
/// Before this function existed, those call sites called `backfill_entity_lookup_keys` directly
/// and, on failure, only flipped the in-process `LookupKeyStatus` flag — never persisting
/// `SchemaState`. That reopened the exact gap `ensure_lookup_key_backfill` was built to close
/// (see its doc comment below), via a different trigger: `migrate()` runs once against an empty
/// `Entity` table and marks `SchemaState` `"complete"`; WAL replay then creates rows with a NULL
/// `lookup_key` via raw Cypher, and if *this* backfill then failed, only the in-process flag
/// noticed. `SchemaState` still said `"complete"`, so the next restart's `migrate()` probe took
/// the O(1) skip path and never retried, leaving NULL-keyed rows invisible to the three
/// no-scan-fallback FR-011 call sites (PR #483 review).
///
/// `ensure_schema_state_table` is called here too, even though every current call site runs
/// after `init_schema`/`migrate` has already created the table once: `CREATE NODE TABLE IF NOT
/// EXISTS` is a cheap catalog check, not a scan, and calling it removes the ordering assumption
/// entirely rather than relying on every future call site getting it right.
pub(crate) fn backfill_entity_lookup_keys_and_record_status(conn: &Conn<'_>) {
    if let Err(e) = ensure_schema_state_table(conn) {
        eprintln!(
            "liminis-context-graph: lookup_key backfill: ensure SchemaState table (non-fatal): {e}"
        );
    }
    run_lookup_key_backfill_and_record_status(conn);
}

/// Ensures `Entity.lookup_key` is fully backfilled, retrying a previously-failed attempt
/// without reintroducing an O(N) `Entity` scan on every clean startup (issue #221, human
/// review on PR #483).
///
/// The gap this closes: `migrate`'s original design only ran `ALTER TABLE Entity ADD
/// lookup_key` (and the backfill after it) when the column was *absent*. If `ALTER` succeeded
/// but the backfill then failed, the column already existed on the next open — the probe would
/// succeed, the whole step would be skipped, and the backfill would never retry. Worse,
/// `LookupKeyStatus::migrated` is an in-process `AtomicBool` that resets to its `true` default
/// on every fresh `Db`, so a restart after a failed backfill would make `knowledge_status`
/// report `name_index_trusted: true` even though rows were still missing `lookup_key` — a real
/// dedup-corruption path for the three FR-011 call sites, not just degraded observability.
///
/// The fix persists the backfill's completion state in `SchemaState` (a point-lookup by primary
/// key, not a scan) instead of re-deriving it from column presence:
/// - Column absent (pre-#221 database): run the `ALTER`, then the backfill, then record the
///   outcome. This is the same one-shot, table-scanning cost the original design always paid.
/// - Column present, marker says `"complete"`: nothing to do — an O(1) point lookup confirms
///   there is nothing left to backfill, exactly the fast path this issue exists to provide.
/// - Column present, marker says `"failed"`: retry the backfill (bounded to `WHERE lookup_key
///   IS NULL`, per `backfill_entity_lookup_keys`) rather than trusting a stale "healthy" signal
///   forever.
/// - Column present, no marker at all: either a genuinely fresh database (its `Entity` table is
///   empty, so the backfill's `WHERE lookup_key IS NULL` scan costs nothing) or a database
///   migrated by a pre-marker build of this feature. Either way this runs the backfill once —
///   for the fresh-DB case it's free; for the pre-marker case it's a one-time cost paid on the
///   first startup after upgrading to this fix, never again once the marker is written.
fn ensure_lookup_key_backfill(conn: &Conn<'_>) {
    if let Err(e) = ensure_schema_state_table(conn) {
        eprintln!(
            "liminis-context-graph: schema migrate: ensure SchemaState table (non-fatal): {e}"
        );
    }

    // Entity gained `kind` (issue #615). A fresh DB has the column from `create_node_tables`;
    // an existing one gets it here, NULL on every row, which the widened backfill below fills
    // (and re-keys) because the v2 marker cannot yet be `complete`.
    if conn
        .raw_query("MATCH (n:Entity) WHERE n.uuid = '_probe_' RETURN n.kind LIMIT 0")
        .is_err()
    {
        if let Err(e) = conn.raw_query("ALTER TABLE Entity ADD kind STRING") {
            eprintln!(
                "liminis-context-graph: schema migrate: ALTER TABLE Entity ADD kind STRING (non-fatal): {e}"
            );
            conn.mark_lookup_key_migration_failed();
            return;
        }
    }

    let lookup_key_column_absent = conn
        .raw_query("MATCH (n:Entity) WHERE n.uuid = '_probe_' RETURN n.lookup_key LIMIT 0")
        .is_err();

    if lookup_key_column_absent {
        if let Err(e) = conn.raw_query("ALTER TABLE Entity ADD lookup_key STRING") {
            eprintln!(
                "liminis-context-graph: schema migrate: ALTER TABLE Entity ADD lookup_key STRING (non-fatal): {e}"
            );
            conn.mark_lookup_key_migration_failed();
            if let Err(e2) = set_schema_state_status(conn, LOOKUP_KEY_BACKFILL_STATE_KEY, "failed")
            {
                eprintln!(
                    "liminis-context-graph: schema migrate: record lookup_key ALTER failure in SchemaState (non-fatal): {e2}"
                );
            }
            return;
        }
        run_lookup_key_backfill_and_record_status(conn);
        return;
    }

    match schema_state_status(conn, LOOKUP_KEY_BACKFILL_STATE_KEY) {
        Ok(Some(status)) if status == "complete" => {
            // Persisted truth: the backfill already ran successfully. O(1) point lookup, no
            // scan — this is the steady-state path every clean startup takes.
        }
        Ok(_) => {
            // Either a persisted "failed" marker (retry) or no marker at all (a fresh DB with
            // nothing to backfill, or a pre-marker database paying its one-time cost).
            run_lookup_key_backfill_and_record_status(conn);
        }
        Err(e) => {
            eprintln!(
                "liminis-context-graph: schema migrate: read SchemaState for lookup_key backfill (non-fatal): {e}"
            );
            conn.mark_lookup_key_migration_failed();
        }
    }
}

/// `SchemaState` key recording which lbug version built the FTS indexes (issue #649, ADR-0649).
/// Its `status` is `lbug::VERSION`. lbug 0.20 → 0.21 changed how non-ASCII FTS terms are stored
/// *without* a storage-version bump, so a 0.20-built index cannot be maintained (deletes fail)
/// or searched (silently empty) by 0.21 — and nothing in the file format reveals it. A missing
/// marker, or one differing from the running lbug, means "rebuild". Written last, after all 3
/// indexes are known to have been built by the running lbug, so a crash leaves it unset.
pub(crate) const FTS_MARKER_KEY: &str = "fts_built_by_lbug";

const CREATE_FTS_SQL: [&str; 3] = [
    "CALL CREATE_FTS_INDEX('Entity', 'node_name_and_summary', ['name', 'summary'])",
    "CALL CREATE_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', ['name', 'fact'])",
    "CALL CREATE_FTS_INDEX('Episodic', 'episode_content', \
     ['content', 'source', 'source_description'])",
];

const DROP_FTS_SQL: [&str; 3] = [
    "CALL DROP_FTS_INDEX('Entity', 'node_name_and_summary')",
    "CALL DROP_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact')",
    "CALL DROP_FTS_INDEX('Episodic', 'episode_content')",
];

/// Creates the 3 FTS indexes. Idempotent — an "already exists" error is swallowed; any other
/// error (missing table, malformed column, ...) propagates so callers can observe a genuine
/// index-build failure instead of silently treating it as success.
/// Index names and covered columns match the upstream Python graphiti-core service (canonical source).
///
/// Also the single detection point for FTS indexes built by an older lbug (issue #649,
/// ADR-0649). Unless the [`FTS_MARKER_KEY`] marker equals the running `lbug::VERSION`:
/// - all 3 indexes were just *created* → they were built by this lbug (fresh database, or
///   dropped by a replay): write the marker, nothing else;
/// - any index *already existed* → it may be stale: drop all 3, recreate all 3, then write
///   the marker (one-time; logged on stderr).
pub(crate) fn create_fts_indexes(conn: &Conn<'_>) -> Result<(), Error> {
    create_fts_indexes_outcome(conn).map(|_| ())
}

/// What [`create_fts_indexes`] concluded; returned so tests can assert *whether* a rebuild ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FtsOutcome {
    /// Marker already matched the running lbug; nothing to do.
    Current,
    /// All 3 indexes were just created by this lbug; the marker was written, no rebuild.
    MarkedFresh,
    /// Pre-existing indexes of unknown provenance were dropped, recreated and marked.
    Rebuilt,
}

fn create_fts_indexes_outcome(conn: &Conn<'_>) -> Result<FtsOutcome, Error> {
    let marker_current = fts_marker_is_current(conn)?;

    let mut already_existed = false;
    for sql in CREATE_FTS_SQL {
        if let Err(e) = conn.raw_query(sql) {
            if !crate::error::is_already_exists_error(&e) {
                return Err(e);
            }
            already_existed = true;
        }
    }
    if marker_current {
        return Ok(FtsOutcome::Current);
    }
    if already_existed {
        eprintln!(
            "liminis-context-graph: rebuilding full-text (FTS) indexes: they were not built by \
             this lbug version ({}). This is a one-time rebuild; its time is proportional to \
             corpus size, and it is needed to recover search and writes for non-ASCII terms \
             (issue #649).",
            lbug::VERSION
        );
        rebuild_fts_indexes(conn)?;
        return Ok(FtsOutcome::Rebuilt);
    }
    write_fts_marker(conn)?;
    Ok(FtsOutcome::MarkedFresh)
}

/// Drops all 3 FTS indexes, recreates them, then records the marker — in that order, so a
/// crash anywhere leaves the marker unset and the next open rebuilds again. All statements are
/// unrecorded (never enter `executed_mutations`, hence never the WAL; ADR-0015). Always
/// rebuilds all 3 together: dropping only the index named in an "inconsistent" error just
/// moves the failure to the next one.
pub(crate) fn rebuild_fts_indexes(conn: &Conn<'_>) -> Result<(), Error> {
    ensure_schema_state_table(conn)?;
    // Clear any *current* marker first (the backstop runs with one set): otherwise a crash after
    // only some of the drops would leave a marker that the next open trusts, skipping the rebuild
    // and leaving an undropped inconsistent index in place (FR-009).
    conn.exec_params_unrecorded(
        "MATCH (s:SchemaState {key: $key}) DELETE s",
        serde_json::json!({ "key": FTS_MARKER_KEY }),
    )?;
    for sql in DROP_FTS_SQL {
        let _ = conn.query_unrecorded(sql);
    }
    for sql in CREATE_FTS_SQL {
        conn.query_unrecorded(sql)?;
    }
    write_fts_marker(conn)
}

/// Whether the marker equals the running lbug. Reads first and only creates `SchemaState` when
/// it is missing: `CREATE NODE TABLE IF NOT EXISTS` is a write transaction (and so a checkpoint)
/// even when it is a no-op, which `build_indices_and_constraints` — called on every replay and
/// recovery — must not pay when nothing needs doing.
fn fts_marker_is_current(conn: &Conn<'_>) -> Result<bool, Error> {
    match schema_state_status(conn, FTS_MARKER_KEY) {
        Ok(v) => Ok(v.as_deref() == Some(lbug::VERSION)),
        Err(e) if crate::error::is_missing_table_error(&e) => {
            ensure_schema_state_table(conn)?;
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

fn write_fts_marker(conn: &Conn<'_>) -> Result<(), Error> {
    conn.exec_params_unrecorded(
        "MERGE (s:SchemaState {key: $key}) SET s.status = $status",
        serde_json::json!({ "key": FTS_MARKER_KEY, "status": lbug::VERSION }),
    )
}

/// Drops the 3 FTS indexes. Idempotent — errors are suppressed so this is safe to call
/// even when the indexes are already absent (e.g. repeated reload or interrupted reload).
/// Used by `handle_rebuild_from_wal` to enable bulk-load replay without inline FTS maintenance.
pub fn drop_fts_indexes(conn: &Conn<'_>) {
    for sql in DROP_FTS_SQL {
        let _ = conn.raw_query(sql);
    }
}

#[cfg(test)]
mod create_fts_indexes_tests {
    use super::*;
    use crate::db::Db;
    use tempfile::TempDir;

    /// Regression guard for issue #192: `create_fts_indexes` must stay idempotent — `init`
    /// already builds these indexes once, so a subsequent explicit call (e.g. the post-reload
    /// `build_indices_and_constraints`) must swallow the "already exists" error and return
    /// `Ok(())`, not propagate it as a genuine failure.
    #[test]
    fn double_create_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        // init_schema (via init()) already created the indexes once; call again explicitly twice more.
        assert!(create_fts_indexes(&conn).is_ok());
        assert!(create_fts_indexes(&conn).is_ok());
    }

    /// Regression guard for issue #192: a genuine failure (target table missing) must propagate
    /// as `Err`, not be silently swallowed as "already exists". Before the fix,
    /// `create_fts_indexes` blanket-suppressed every error and always returned `Ok(())`.
    #[test]
    fn missing_table_returns_genuine_error() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        // No init_schema() — Entity/Episodic/RelatesToNode_ tables don't exist.
        let err = create_fts_indexes(&conn).expect_err("must fail when target tables don't exist");
        assert!(
            !crate::error::is_already_exists_error(&err),
            "missing-table error must not be misclassified as already-exists: {err}"
        );
    }
}

#[cfg(test)]
mod fts_marker_tests {
    use super::*;
    use crate::db::Db;
    use tempfile::TempDir;

    fn marker(conn: &Conn<'_>) -> Option<String> {
        schema_state_status(conn, FTS_MARKER_KEY).unwrap()
    }

    fn clear_marker(conn: &Conn<'_>) {
        conn.exec_params_unrecorded(
            "MATCH (s:SchemaState {key: $key}) DELETE s",
            serde_json::json!({ "key": FTS_MARKER_KEY }),
        )
        .unwrap();
    }

    fn open() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        (dir, db)
    }

    /// SC-003: a database created by this build writes the marker at index creation and a later
    /// open does not rebuild.
    #[test]
    fn fresh_db_writes_marker_and_does_not_rebuild() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::Current
        );
    }

    /// SC-002 / FR-003: indexes present but no marker (every pre-fix database) → rebuild once;
    /// the next call does not rebuild.
    #[test]
    fn missing_marker_rebuilds_once() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        clear_marker(&conn);
        assert_eq!(marker(&conn), None);
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::Rebuilt
        );
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::Current
        );
    }

    /// FR-009: `rebuild_fts_indexes` clears the marker before its first drop and re-sets it
    /// last, whether or not a marker was present when it started.
    #[test]
    fn rebuild_with_current_marker_or_none_ends_marked() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
        rebuild_fts_indexes(&conn).unwrap();
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
        clear_marker(&conn);
        rebuild_fts_indexes(&conn).unwrap();
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
    }

    #[test]
    fn mismatched_marker_rebuilds() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        set_schema_state_status(&conn, FTS_MARKER_KEY, "0.20.3").unwrap();
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::Rebuilt
        );
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
    }

    /// Indexes dropped (as a replay does) and no marker → the creates all succeed, so the
    /// indexes are known-fresh: marker only, no rebuild.
    #[test]
    fn all_created_writes_marker_without_rebuild() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        drop_fts_indexes(&conn);
        clear_marker(&conn);
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::MarkedFresh
        );
        assert_eq!(marker(&conn).as_deref(), Some(lbug::VERSION));
    }

    /// ADR-0015: the rebuild's DDL and the marker write must never enter `executed_mutations`
    /// (handlers drain that buffer into the WAL).
    #[test]
    fn rebuild_is_not_recorded_for_the_wal() {
        let (_d, db) = open();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        clear_marker(&conn);
        let _ = conn.drain_mutations();
        assert_eq!(
            create_fts_indexes_outcome(&conn).unwrap(),
            FtsOutcome::Rebuilt
        );
        let recorded = conn.drain_mutations();
        assert!(recorded.is_empty(), "unexpected WAL entries: {recorded:?}");
    }
}

/// Every per-group WAL directory under `wal_root` — the `wal_dirs` argument for the
/// `ingested_at` backfill at sites that rebuild or reopen the whole embedded DB (which holds
/// every group's records, not just the default group's).
pub fn group_wal_dirs(wal_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    crate::wal_group::list_group_wal_dirs(wal_root)
        .map(|v| v.into_iter().map(|(_, d)| d).collect())
        .unwrap_or_default()
}
