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
         lookup_key STRING\
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
    // time against the running embedder's identity to surface a mismatch (FR-008), independent
    // of the write-time `.wal-embedding-model.json` sidecar (`wal_embedding_identity`), which
    // answers a different question ("what did this WAL claim") from this one ("what does the
    // graph actually contain now").
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
    let rows = conn.query_params(
        "MATCH (n:Entity) WHERE n.lookup_key IS NULL RETURN n.uuid, n.name, n.group_id",
        serde_json::json!({}),
    )?;
    for row in rows {
        let uuid = crate::db::value_as_string(&row[0]);
        let name = crate::db::value_as_string(&row[1]);
        let group_id = crate::db::value_as_string(&row[2]);
        let key = crate::db::compute_lookup_key(&group_id, &name);
        conn.exec_params(
            "MATCH (n:Entity {uuid: $uuid}) SET n.lookup_key = $key",
            serde_json::json!({ "uuid": uuid, "key": key }),
        )?;
    }
    Ok(())
}

/// Key under which the `lookup_key` backfill's completion state is persisted in `SchemaState`
/// (see `ensure_lookup_key_backfill`).
const LOOKUP_KEY_BACKFILL_STATE_KEY: &str = "entity_lookup_key_backfill";

/// A minimal, generic migration-state marker table: one row per named migration step, keyed by
/// a stable string identifier. Introduced by issue #221 to close a gap the PR's own human review
/// caught — see `ensure_lookup_key_backfill`'s doc comment for the failure mode this exists to
/// prevent. `CREATE NODE TABLE IF NOT EXISTS` is a catalog check, not a scan, so calling this
/// unconditionally on every `migrate()` run is cheap regardless of database size or age.
fn ensure_schema_state_table(conn: &Conn<'_>) -> Result<(), Error> {
    conn.raw_query(
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

/// Creates the 3 FTS indexes. Idempotent — an "already exists" error is swallowed; any other
/// error (missing table, malformed column, ...) propagates so callers can observe a genuine
/// index-build failure instead of silently treating it as success.
/// Index names and covered columns match the upstream Python graphiti-core service (canonical source).
pub(crate) fn create_fts_indexes(conn: &Conn<'_>) -> Result<(), Error> {
    for sql in [
        "CALL CREATE_FTS_INDEX('Entity', 'node_name_and_summary', ['name', 'summary'])",
        "CALL CREATE_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', ['name', 'fact'])",
        "CALL CREATE_FTS_INDEX('Episodic', 'episode_content', \
         ['content', 'source', 'source_description'])",
    ] {
        if let Err(e) = conn.raw_query(sql) {
            if !crate::error::is_already_exists_error(&e) {
                return Err(e);
            }
        }
    }
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
