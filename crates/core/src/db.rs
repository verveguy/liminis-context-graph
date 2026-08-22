use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use lbug::{LogicalType, Value};

use crate::{
    error::Error,
    name_index::NameIndex,
    pointer::EndpointSide,
    types::{EntityRow, EpisodicRow, MentionsEdge, PassageResult, RelatesToEdge},
};

/// Map from entity UUID to (episode_uuids, source_descriptions), positionally aligned.
type EpisodeInfoMap = HashMap<String, (Vec<String>, Vec<String>)>;

/// Mutations drained from a `Conn` and bucketed by the `group_id` whose data they modify
/// (issue #385). Used by the two handlers that are multi-group by design — `group_purge` and
/// `cross_group::rebind_pointers_impl` — so each group's mutations can be flushed to that
/// group's own WAL stream instead of all landing on one caller-named group (FR-001).
pub type GroupedMutations = BTreeMap<String, Vec<(String, serde_json::Value)>>;

pub struct Db {
    inner: lbug::Database,
    /// In-process accelerator for `Conn::get_entity_by_name_ci` (issue #219). Lives on `Db`
    /// (not `Conn`) because it must survive across requests; `AppState.db` is swapped
    /// wholesale on `clear_all`/recovery, so a fresh `Db` naturally starts with an empty
    /// index. See `name_index.rs` and the NameIndex ADR for the invalidation contract.
    name_index: NameIndex,
}

/// The persisted consumer-side WAL position for one group (issue #353, made per-group by
/// issue #378, scoped to a generation by issue #387): `applied_seq` and the generation it was
/// recorded against, always read/written together as a single row so a caller can never see one
/// without the other — splitting them would reintroduce the two-read/write coordination problem
/// ADR-0353's single-row design was built to avoid (see ADR-0387).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalPositionRecord {
    /// `None` iff no position has ever been recorded for this group. Row-absence is the sole
    /// representation of "unknown" — both "never written" (fresh group, backfill not yet run)
    /// and "backfill failed" collapse to this, distinct from a written `0` ("nothing applied
    /// yet, but known").
    pub applied_seq: Option<u64>,
    /// The generation `applied_seq` was recorded against. `None` means either no position has
    /// ever been recorded, or the position was recorded before issue #387 (a pre-existing stream
    /// with no generation concept yet) — both collapse to "unknown," never treated as a mismatch
    /// (FR-009).
    pub generation: Option<String>,
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
    name_index: &'db NameIndex,
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
        Ok(Self {
            inner,
            name_index: NameIndex::default(),
        })
    }

    /// If `db_path` is absent but `wal_dir` contains `.jsonl` files, creates a fresh DB and
    /// replays the WAL to rebuild it (R-06). Otherwise behaves like `Db::open`.
    ///
    /// Returns the `ReplayStats` from the rebuild alongside the `Db` (FR-001) — `Some` when a
    /// rebuild ran, `None` when it didn't (the DB already existed, or no WAL was present). When
    /// the rebuild's `fidelity_warning` is set, this also emits a `[WAL REBUILD WARNING]` line
    /// (FR-002) so the outcome is observable even if the caller ignores the returned stats —
    /// `open_or_rebuild` is pure `crates/core` library code with no `TelemetrySink`/logging
    /// framework available, so a bracketed-tag `eprintln!` matches this codebase's existing
    /// `[WAL WARN]`/`[WAL PROGRESS]` convention.
    pub fn open_or_rebuild(
        db_path: &str,
        wal_dir: &str,
        embedding_dim: usize,
    ) -> Result<(Self, Option<crate::replay::ReplayStats>), Error> {
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

        let stats = if !db_exists && has_wal {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            let stats = crate::replay::WalReplayer::new(wal_dir).replay(&conn)?;
            if let Some(ref warning) = stats.fidelity_warning {
                eprintln!("[WAL REBUILD WARNING] {warning}");
            }
            // WAL replay executes raw recorded Cypher templates, bypassing the typed
            // insert_entity/update_entity_created_at hooks — a full rebuild is the only
            // way the name index observes replayed data (FR-004).
            conn.rebuild_name_index()?;
            // Persist the applied-WAL-seq position (issue #353, FR-004) at the precise value
            // the replay just computed — a fresh rebuild is exactly as authoritative as
            // knowledge_rebuild_from_wal's own post-replay write. Non-fatal: a missed write
            // only means the boot-time check falls back to a rescan, not that the rebuild
            // itself is compromised.
            if let Some(seq) = stats.last_committed_seq {
                // `open_or_rebuild` replays exactly one WAL directory into a fresh DB — there is
                // no notion of multiple groups at this call site, so the position it persists is
                // the default group's (issue #378 FR-009: single-group parity). The generation
                // persisted alongside it is whatever is currently on disk for that directory
                // (issue #387) — `None` for a pre-#387 stream, matching FR-009's adopt-on-first-
                // encounter semantics.
                let generation = crate::wal_generation::read_generation(wal_dir_path);
                if let Err(e) = conn.set_wal_position(
                    crate::wal_group::DEFAULT_GROUP_ID,
                    seq,
                    generation.as_deref(),
                ) {
                    eprintln!(
                        "liminis-context-graph: open_or_rebuild: failed to persist applied_seq={seq} (non-fatal): {e}"
                    );
                }
            }
            Some(stats)
        } else {
            // No rebuild ran (DB already existed, or a genuinely fresh DB with no WAL to
            // replay). init_schema is idempotent (CREATE ... IF NOT EXISTS), so it's safe to
            // call unconditionally here — previously this branch left a genuinely fresh DB
            // (no WAL to replay either) with no schema at all.
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            // Carry a pre-378 database's WalPosition {id: 'singleton'} row forward to the
            // default group's own row (issue #378 FR-001/FR-009) before the backfill check below
            // decides whether a position is already known. No-op on a fresh DB (no legacy row)
            // or after the first call (idempotent).
            if let Err(e) =
                conn.migrate_legacy_singleton_wal_position(crate::wal_group::DEFAULT_GROUP_ID)
            {
                eprintln!(
                    "liminis-context-graph: open_or_rebuild: legacy singleton WalPosition migration failed (non-fatal): {e}"
                );
            }
            // Backfill applied_seq for a pre-existing populated DB that predates this feature
            // (FR-007), or set it to 0 for a genuinely fresh DB. Non-fatal: a missed backfill
            // just leaves the boot-time check reporting null (safe — the documented action is
            // a full rebuild).
            if let Err(e) = crate::recovery::backfill_applied_seq_if_absent(
                &conn,
                crate::wal_group::DEFAULT_GROUP_ID,
                wal_dir_path,
            ) {
                eprintln!(
                    "liminis-context-graph: open_or_rebuild: applied_seq backfill failed (non-fatal): {e}"
                );
            }
            None
        };

        Ok((db, stats))
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
            name_index: &self.name_index,
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

    /// Issues a transaction-control statement (`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK`) via
    /// `Connection::query`, without recording anything into `executed_mutations`.
    ///
    /// Deliberately distinct from [`Conn::raw_query`]: `raw_query` records every call into
    /// `executed_mutations`, a buffer meant for live-write WAL logging that nothing drains for a
    /// replay connection — using it here would accumulate one entry per transaction for the life
    /// of a multi-hour replay, an unbounded-memory regression this issue exists to avoid (User
    /// Story 4), not to introduce in a different place. Used only by WAL replay's `flush_batch`.
    pub(crate) fn exec_transaction_control(&self, sql: &str) -> Result<(), Error> {
        let _ = self.inner.query(sql)?;
        Ok(())
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

    /// Like [`Conn::execute_prepared`], but returns the row count reported by the query instead
    /// of discarding it. Used only by WAL replay's `MATCH`-prefixed no-op detection (FR-005):
    /// the caller prepares the template with a `RETURN count(*)` probe appended (see
    /// `replay::with_match_count_probe`) so a `MATCH ... SET`/`DETACH DELETE` that matched zero
    /// rows is distinguishable from one that matched and applied. Not used by the live-write
    /// path (`exec_params`), which is unaffected by this addition.
    ///
    /// Falls back to `1` (i.e. "treat as matched") when the result's first row/column isn't a
    /// countable numeric type — this fails toward not inflating the no-op counter rather than
    /// toward silently losing writes, in case a future lbug version changes `count(*)`'s result
    /// shape.
    pub(crate) fn execute_prepared_returning_count(
        &self,
        prepared: &mut lbug::PreparedStatement,
        params: &serde_json::Value,
    ) -> Result<i64, Error> {
        let (keys, vals): (Vec<String>, Vec<Value>) =
            json_params_to_values(params).into_iter().unzip();
        let bound: Vec<(&str, Value)> = keys.iter().map(|k| k.as_str()).zip(vals).collect();
        let result = self.inner.execute(prepared, bound)?;
        let rows: Vec<Vec<Value>> = result.collect();
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .map(value_as_match_count)
            .unwrap_or(1))
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

    /// Drains mutations recorded since the last drain and merges them into `grouped`'s
    /// `group_id` bucket (issue #385: per-group mutation attribution for `group_purge` and
    /// `cross_group::rebind_pointers_impl`, the two handlers that are multi-group by design).
    /// A no-op if nothing was recorded — an empty bucket is never created, matching
    /// `wal_exec::wal_flush_ungrouped`'s own "empty mutations ⇒ no WAL directory" behavior.
    pub fn drain_mutations_into(&self, grouped: &mut GroupedMutations, group_id: &str) {
        let drained = self.drain_mutations();
        if !drained.is_empty() {
            grouped
                .entry(group_id.to_string())
                .or_default()
                .extend(drained);
        }
    }

    /// Creates the Entity and Episodic node tables. Call once after connecting.
    pub fn init_schema(&self, embedding_dim: usize) -> Result<(), Error> {
        crate::schema::init(self, embedding_dim)?;
        crate::schema::migrate(self, embedding_dim);
        Ok(())
    }

    /// Creates HNSW vector indexes and FTS indexes; idempotent.
    pub fn build_indices_and_constraints(&self) -> Result<(), Error> {
        self.create_vector_indexes()?;
        crate::schema::create_fts_indexes(self)
    }

    /// Repopulates the in-process name-lookup index (issue #219) from a full `Entity` table
    /// scan. This is the FR-004 single-scan mechanism, and the only way the index observes
    /// `Entity` rows written by a path that bypasses the typed `insert_entity`/
    /// `update_entity_created_at` methods — most importantly WAL replay, which executes raw
    /// recorded Cypher templates. Call at startup, after any recovery strategy, and after
    /// any non-dry-run WAL rebuild. Idempotent — always a full replace, never additive.
    pub fn rebuild_name_index(&self) -> Result<(), Error> {
        let rows = self.query_params(
            "MATCH (e:Entity) RETURN e.uuid, e.name, e.group_id, e.created_at",
            serde_json::json!({}),
        )?;
        let entries = rows
            .into_iter()
            .map(|row| {
                (
                    value_as_string(&row[0]),
                    value_as_string(&row[1]),
                    value_as_string(&row[2]),
                    value_as_timestamp_str(&row[3]),
                )
            })
            .collect();
        self.name_index.rebuild(entries);
        Ok(())
    }

    // ── Entity/Episodic insert ─────────────────────────────────────────────────

    pub fn insert_entity(&self, row: &EntityRow) -> Result<(), Error> {
        // Enforce Entity-first label-order invariant (AD-8)
        let labels = enforce_entity_first(&row.labels);
        // `summary_embedding` is a fixed-size `FLOAT[N]` column, same as `name_embedding` above
        // — a zero-length list fails to bind ("Unsupported casting LIST with incorrect list
        // entry to ARRAY"). Callers that don't compute a real summary embedding (an empty
        // `EntityRow::default()`-derived value, which every pre-#470 call site produces) get a
        // same-dimension zero vector here, sized off `name_embedding` since that field is always
        // populated with a real, correctly-sized vector by every caller.
        let summary_embedding = if row.summary_embedding.is_empty() {
            vec![0.0f32; row.name_embedding.len()]
        } else {
            row.summary_embedding.clone()
        };
        self.exec_params(
            "CREATE (:Entity {uuid: $uuid, name: $name, group_id: $group_id, \
             labels: $labels, created_at: $created_at, name_embedding: $name_embedding, \
             summary: $summary, attributes: $attributes, \
             summary_embedding: $summary_embedding})",
            serde_json::json!({
                "uuid": row.uuid,
                "name": row.name,
                "group_id": row.group_id,
                "labels": labels,
                "created_at": row.created_at,
                "name_embedding": row.name_embedding,
                "summary": row.summary,
                "attributes": row.attributes,
                "summary_embedding": summary_embedding,
            }),
        )?;
        self.name_index
            .insert(&row.uuid, &row.name, &row.group_id, &row.created_at);
        Ok(())
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

    /// Inserts a cross-group `RelatesToEdge` (issue #369) whose foreign endpoint(s) may be
    /// currently unresolved — `source_node_uuid`/`target_node_uuid` is the empty string
    /// sentinel for `Unbound`/`Ambiguous` (see `cross_group::resolve_endpoint`), never a valid
    /// entity UUID.
    ///
    /// Unlike `insert_relates_to_edge`'s single all-or-nothing three-statement shape (a `MATCH`
    /// that fails to bind silently creates zero rows), each hop here is created independently
    /// and only when that side's UUID is non-empty — so a foreign endpoint that doesn't
    /// currently resolve leaves only that hop absent (FR-004) rather than blocking the whole
    /// insert. `MERGE` (not `CREATE`) makes every statement safe to re-run, which is what makes
    /// `cross_group::rebind_pointers` idempotent (FR-009). `insert_relates_to_edge` itself is
    /// left byte-for-byte unchanged — this is a separate function specifically so the hot
    /// intra-group insert path pays zero cost for this feature (SC-004).
    pub fn insert_cross_group_edge(&self, edge: &RelatesToEdge) -> Result<(), Error> {
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

        if !edge.source_node_uuid.is_empty() && !edge.target_node_uuid.is_empty() {
            self.create_relates_to_direct(edge)?;
        }

        if !edge.source_node_uuid.is_empty() {
            self.create_relates_to_hop(&edge.uuid, EndpointSide::Src, &edge.source_node_uuid)?;
        }
        if !edge.target_node_uuid.is_empty() {
            self.create_relates_to_hop(&edge.uuid, EndpointSide::Dst, &edge.target_node_uuid)?;
        }
        Ok(())
    }

    /// Creates (via `MERGE` on the rel's `uuid` property) the direct `Entity→Entity` compat rel
    /// — matching `insert_relates_to_edge`'s second statement — only meaningful once both
    /// endpoints resolve. Shared by `insert_cross_group_edge` (initial creation, when both sides
    /// already resolve) and `cross_group::rebind_pointers` (when a pointer transitions into
    /// `Bound` and its counterpart side is already resolved too). Callers whose src/dst may have
    /// changed since a prior direct rel was created should call [`Self::delete_relates_to_direct`]
    /// first — `MERGE` matches on `(src, uuid, dst)` together, so a stale rel pointing at a
    /// *different* dst/src is not found and would be left behind rather than replaced.
    pub fn create_relates_to_direct(&self, edge: &RelatesToEdge) -> Result<(), Error> {
        self.exec_params(
            "MATCH (src:Entity {uuid: $src}), (dst:Entity {uuid: $dst}) \
             MERGE (src)-[:RELATES_TO {uuid: $uuid, name: $name, group_id: $group_id, \
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
        )
    }

    /// Removes the direct `Entity→Entity` compat rel for the given edge uuid, if present. The
    /// rel is always created `src→dst` (never the reverse), so this matches that one direction
    /// only — consistent with [`Self::create_relates_to_direct`]. Used by
    /// `cross_group::rebind_pointers` before re-syncing the compat rel (a stale rel may point at
    /// a since-changed src/dst) and when a previously-`Bound` pointer loses resolution (the
    /// compat rel, like the two-hop model, should not survive that).
    pub fn delete_relates_to_direct(&self, rn_uuid: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (src:Entity)-[r:RELATES_TO {uuid: $uuid}]->(dst:Entity) DELETE r",
            serde_json::json!({ "uuid": rn_uuid }),
        )
    }

    /// Creates (idempotently, via `MERGE`) one `RelatesToNode_ -[:RELATES_TO]- Entity` hop in
    /// the given direction. Shared by `insert_cross_group_edge` (initial creation) and
    /// `cross_group::rebind_pointers` (re-creating a hop after a pointer resolves).
    pub fn create_relates_to_hop(
        &self,
        rn_uuid: &str,
        side: EndpointSide,
        entity_uuid: &str,
    ) -> Result<(), Error> {
        match side {
            EndpointSide::Src => self.exec_params(
                "MATCH (src:Entity {uuid: $src}), (rn:RelatesToNode_ {uuid: $rn}) \
                 MERGE (src)-[:RELATES_TO]->(rn)",
                serde_json::json!({ "src": entity_uuid, "rn": rn_uuid }),
            ),
            EndpointSide::Dst => self.exec_params(
                "MATCH (rn:RelatesToNode_ {uuid: $rn}), (dst:Entity {uuid: $dst}) \
                 MERGE (rn)-[:RELATES_TO]->(dst)",
                serde_json::json!({ "rn": rn_uuid, "dst": entity_uuid }),
            ),
        }
    }

    /// Removes an existing `RelatesToNode_ -[:RELATES_TO]- Entity` hop in the given direction,
    /// if present. Used by `cross_group::rebind_pointers` to drop a stale hop before creating
    /// the (possibly different) resolved one — e.g. a source rename or re-extraction under a
    /// new UUID generation leaves the old hop pointing at a UUID that no longer names the same
    /// entity, or at a UUID that no longer exists at all.
    pub fn delete_relates_to_hop(&self, rn_uuid: &str, side: EndpointSide) -> Result<(), Error> {
        match side {
            EndpointSide::Src => self.exec_params(
                "MATCH (src:Entity)-[r:RELATES_TO]->(rn:RelatesToNode_ {uuid: $rn}) DELETE r",
                serde_json::json!({ "rn": rn_uuid }),
            ),
            EndpointSide::Dst => self.exec_params(
                "MATCH (rn:RelatesToNode_ {uuid: $rn})-[r:RELATES_TO]->(dst:Entity) DELETE r",
                serde_json::json!({ "rn": rn_uuid }),
            ),
        }
    }

    /// Overwrites `RelatesToNode_.attributes` for the given edge. Used by
    /// `cross_group::rebind_pointers` to persist a re-resolved pointer's new
    /// `resolved_uuid`/`bound_at_seq`/`binding_state` back into the JSON column.
    pub fn update_relates_to_attributes(&self, uuid: &str, attributes: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (rn:RelatesToNode_ {uuid: $uuid}) SET rn.attributes = $attributes",
            serde_json::json!({ "uuid": uuid, "attributes": attributes }),
        )
    }

    /// Updates an existing `RelatesToNode_` shadow node's `fact`, `valid_at`, `relation_type`,
    /// and `attributes` in a single `SET` statement, then best-effort syncs the same
    /// `fact`/`valid_at`/`attributes` onto the direct `Entity→Entity` compat rel — reusing
    /// `invalidate_edge`'s non-fatal `SET`-on-rel pattern, since lbug 0.17.0 may not support
    /// `SET` on rel properties and a failure here must not fail the whole update. Used by
    /// `knowledge_assert_relationship`'s update-in-place path (issue #379 FR-017): no existing
    /// setter covers `fact`/`valid_at`/`relation_type` in one shot.
    /// `relation_type`/`valid_at` bind as JSON null when `None`, matching `insert_relates_to_edge`.
    ///
    /// Deliberately does **not** touch `fact_embedding`, for the same reason
    /// [`Self::update_entity_core`] doesn't touch `name_embedding`: lbug's HNSW vector index
    /// (built over `RelatesToNode_.fact_embedding`) rejects a plain `SET` on an indexed column —
    /// "Try delete and then insert." The caller still generates a fresh fact embedding on every
    /// call for the `embedding_warning` fallback to stay observable, but only the create path
    /// (`insert_relates_to_edge`, before any index exists over the row) persists it.
    pub fn update_relates_to_core(
        &self,
        uuid: &str,
        fact: &str,
        valid_at: Option<&str>,
        relation_type: Option<&str>,
        attributes: &str,
    ) -> Result<(), Error> {
        self.exec_params(
            "MATCH (rn:RelatesToNode_ {uuid: $uuid}) SET rn.fact = $fact, \
             rn.valid_at = $valid_at, rn.relation_type = $relation_type, \
             rn.attributes = $attributes",
            serde_json::json!({
                "uuid": uuid,
                "fact": fact,
                "valid_at": valid_at,
                "relation_type": relation_type,
                "attributes": attributes,
            }),
        )?;
        if let Err(e) = self.exec_params(
            "MATCH (src:Entity)-[r:RELATES_TO {uuid: $uuid}]->(dst:Entity) \
             SET r.fact = $fact, r.valid_at = $valid_at, r.attributes = $attributes",
            serde_json::json!({
                "uuid": uuid,
                "fact": fact,
                "valid_at": valid_at,
                "attributes": attributes,
            }),
        ) {
            eprintln!(
                "liminis-context-graph: SET fact/valid_at/attributes on RELATES_TO rel unsupported or failed (non-fatal): {e}"
            );
        }
        Ok(())
    }

    /// Lists `(uuid, name, group_id, attributes)` for every non-invalidated `RelatesToNode_`
    /// row carrying at least one cross-group pointer, regardless of which source group it
    /// points into — `cross_group::rebind_pointers` filters this candidate set down to
    /// pointers matching its target source group. A `CONTAINS` pre-filter narrows the scan to
    /// nodes carrying the `cross_group_pointers` key at all; full-table, but acceptable since
    /// this is admin-triggered, not on the hot read/write path (mirrors
    /// `count_cross_group_pointers`).
    pub fn list_cross_group_pointer_candidates(
        &self,
    ) -> Result<Vec<(String, String, String, String)>, Error> {
        let rows = self.query_params(
            "MATCH (rn:RelatesToNode_) \
             WHERE rn.attributes CONTAINS '\"cross_group_pointers\"' AND rn.invalid_at IS NULL \
             RETURN rn.uuid, rn.name, rn.group_id, rn.attributes",
            serde_json::json!({}),
        )?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    value_as_string(&row[0]),
                    value_as_string(&row[1]),
                    value_as_string(&row[2]),
                    value_as_string(&row[3]),
                )
            })
            .collect())
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

    /// Creates HNSW vector indexes on Entity, Episodic, and RelatesToNode_. Idempotent — an
    /// "already exists" error (e.g. a repeat call, or one following `init_schema`) is swallowed;
    /// any other error (missing table, malformed column, ...) propagates so callers can observe a
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
        self.create_entity_summary_embedding_index()
    }

    /// Drops the 4 HNSW vector indexes. Idempotent — errors (including "no such index") are
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
        self.drop_entity_summary_embedding_index();
    }

    /// Creates just the `Entity.summary_embedding` HNSW index. Idempotent, following the same
    /// "already exists" suppression as `create_vector_indexes`. Broken out as its own function
    /// (rather than folded only into the aggregate) so `knowledge_backfill_summary_embeddings`
    /// can drop/rebuild this single index around its write phase without touching the other 3
    /// (issue #470).
    pub fn create_entity_summary_embedding_index(&self) -> Result<(), Error> {
        if let Err(e) = self.raw_query(
            "CALL CREATE_VECTOR_INDEX('Entity', 'entity_summary_embedding_idx', \
             'summary_embedding', metric := 'cosine')",
        ) {
            if !crate::error::is_already_exists_error(&e) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Drops just the `Entity.summary_embedding` HNSW index. Idempotent — errors are suppressed.
    /// See `create_entity_summary_embedding_index` for why this is independently callable.
    pub fn drop_entity_summary_embedding_index(&self) {
        let _ = self.raw_query("CALL DROP_VECTOR_INDEX('Entity', 'entity_summary_embedding_idx')");
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
    ///
    /// Only ever `DETACH DELETE`s `Episodic` nodes, never `Entity` nodes — so this never
    /// invalidates the `NameIndex` (issue #219, ADR-0038). If a future change makes this
    /// (or any path) delete `Entity` nodes, it must also invalidate the corresponding
    /// `NameIndex` entries. `crate::group_purge` (issue #361) is the deliberate, sole
    /// exception to this rule — see `delete_entities_by_group_ids` below.
    pub fn remove_episode(&self, episode_uuid: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (ep:Episodic {uuid: $uuid}) DETACH DELETE ep",
            serde_json::json!({ "uuid": episode_uuid }),
        )
    }

    /// Returns `episode_uuid`'s own `group_id`, or `None` if no such episode exists. Used by
    /// `handle_delete_episode` (issue #378 FR-004) to route that single episode's DELETE
    /// mutation to its *own* group's WAL writer rather than the default group's — unlike the
    /// FR-004-documented default-group-fallback sites, this call targets exactly one episode in
    /// exactly one group, and that group is knowable by reading it before the delete runs, so
    /// misrouting here isn't an inherent limitation of the operation.
    pub fn get_episode_group_id(&self, episode_uuid: &str) -> Result<Option<String>, Error> {
        let rows = self.query_params(
            "MATCH (ep:Episodic {uuid: $uuid}) RETURN ep.group_id",
            serde_json::json!({ "uuid": episode_uuid }),
        )?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| value_as_optional_string(&row[0])))
    }

    /// Deletes all Episodic nodes whose source_description equals source_file or starts with
    /// source_file + ":", scoped to the given group_ids. Returns the UUIDs of deleted episodes.
    ///
    /// group_ids is mandatory and must be non-empty (issue #406) — an unscoped, all-groups
    /// query is not representable here; callers must resolve a group scope before calling.
    /// Returns `Error::Ipc` if `group_ids` is empty, as defense in depth against a future
    /// caller bypassing the handler-layer validation.
    ///
    /// Only ever `DETACH DELETE`s `Episodic` nodes, never `Entity` nodes — so this never
    /// invalidates the `NameIndex` (issue #219, ADR-0038). See `remove_episode`.
    pub fn remove_episodes_by_source(
        &self,
        source_file: &str,
        group_ids: &[&str],
    ) -> Result<Vec<String>, Error> {
        if group_ids.is_empty() {
            return Err(Error::Ipc(
                "remove_episodes_by_source requires a non-empty group_ids".to_string(),
            ));
        }
        let prefix = format!("{}:", source_file);
        let match_sql = "MATCH (ep:Episodic) WHERE (ep.source_description = $src \
             OR ep.source_description STARTS WITH $prefix) AND ep.group_id IN $gids \
             RETURN ep.uuid";
        let params = serde_json::json!({ "src": source_file, "prefix": prefix, "gids": group_ids });
        let uuids: Vec<String> = self
            .query_params(match_sql, params)?
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

    /// Deletes all Episodic nodes whose name (chunk identifier) matches chunk_id, scoped to the
    /// given group_ids. Returns the UUIDs of deleted episodes.
    ///
    /// Matches on ep.name (which always stores chunk_id) rather than source_description.
    /// Orphaned entities connected only to the deleted episodes are NOT removed — callers
    /// should be aware that entity nodes may become disconnected after this call.
    ///
    /// group_ids is mandatory and must be non-empty (issue #406) — an unscoped, all-groups
    /// query is not representable here; callers must resolve a group scope before calling.
    /// Returns `Error::Ipc` if `group_ids` is empty, as defense in depth against a future
    /// caller bypassing the handler-layer validation.
    ///
    /// Only ever `DETACH DELETE`s `Episodic` nodes, never `Entity` nodes — so this never
    /// invalidates the `NameIndex` (issue #219, ADR-0038). See `remove_episode`.
    pub fn remove_episodes_by_chunk_id(
        &self,
        chunk_id: &str,
        group_ids: &[&str],
    ) -> Result<Vec<String>, Error> {
        if group_ids.is_empty() {
            return Err(Error::Ipc(
                "remove_episodes_by_chunk_id requires a non-empty group_ids".to_string(),
            ));
        }
        let match_sql =
            "MATCH (ep:Episodic) WHERE ep.name = $name AND ep.group_id IN $gids RETURN ep.uuid";
        let params = serde_json::json!({ "name": chunk_id, "gids": group_ids });
        let uuids: Vec<String> = self
            .query_params(match_sql, params)?
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

    // ── Group-scoped purge (issue #361) ─────────────────────────────────────────
    //
    // These are the only other places, besides `crate::group_purge`, that delete `Entity` or
    // `RelatesToNode_` nodes — see the warning on `remove_episode` above. Every query here is
    // scoped by `group_id IN $gids` and nothing else, which is what makes FR-008 (a
    // `RelatesToNode_` owned by a group outside `$gids` is never touched) hold by construction:
    // a node whose own `group_id` isn't in the list is simply never matched.

    /// Returns the count of `Entity` nodes in the given group_ids.
    pub fn count_entities_by_group_ids(&self, group_ids: &[&str]) -> Result<u64, Error> {
        self.count_by_group_ids("Entity", "e", group_ids)
    }

    /// Returns the count of `Episodic` nodes in the given group_ids.
    pub fn count_episodics_by_group_ids(&self, group_ids: &[&str]) -> Result<u64, Error> {
        self.count_by_group_ids("Episodic", "ep", group_ids)
    }

    /// Returns the count of `RelatesToNode_` nodes (i.e. RELATES_TO edges, see
    /// `count_relates_to_edges`) owned by the given group_ids. Scoped by the edge's own
    /// `group_id`, not either endpoint's — matching `get_edges_by_group_ids`.
    pub fn count_relates_to_by_group_ids(&self, group_ids: &[&str]) -> Result<u64, Error> {
        self.count_by_group_ids("RelatesToNode_", "rn", group_ids)
    }

    fn count_by_group_ids(&self, label: &str, var: &str, group_ids: &[&str]) -> Result<u64, Error> {
        let sql = format!("MATCH ({var}:{label}) WHERE {var}.group_id IN $gids RETURN count(*)");
        let rows = self.query_params(&sql, serde_json::json!({ "gids": group_ids }))?;
        for row in rows {
            match &row[0] {
                lbug::Value::Int64(n) => return Ok(*n as u64),
                lbug::Value::UInt64(n) => return Ok(*n),
                lbug::Value::Int32(n) => return Ok(*n as u64),
                _ => {}
            }
        }
        Ok(0)
    }

    /// `DETACH DELETE`s every `Entity` node in the given group_ids. This is the deliberate,
    /// sole-with-`delete_relates_to_by_group_ids` exception to the "only ever `Episodic`" rule
    /// documented on `remove_episode` — callers MUST also invalidate/rebuild the `NameIndex`
    /// (see `mark_name_index_untrusted`/`rebuild_name_index`) after calling this.
    pub fn delete_entities_by_group_ids(&self, group_ids: &[&str]) -> Result<(), Error> {
        self.exec_params(
            "MATCH (e:Entity) WHERE e.group_id IN $gids DETACH DELETE e",
            serde_json::json!({ "gids": group_ids }),
        )
    }

    /// `DETACH DELETE`s every `Episodic` node in the given group_ids.
    pub fn delete_episodics_by_group_ids(&self, group_ids: &[&str]) -> Result<(), Error> {
        self.exec_params(
            "MATCH (ep:Episodic) WHERE ep.group_id IN $gids DETACH DELETE ep",
            serde_json::json!({ "gids": group_ids }),
        )
    }

    /// `DETACH DELETE`s every `RelatesToNode_` node *owned by* the given group_ids (i.e. whose
    /// own `rn.group_id` is in the list) — never one merely hopped-to by an `Entity` being
    /// deleted elsewhere. A cross-group edge's `RelatesToNode_` belongs to the layer group that
    /// asserted it, not to either endpoint's group (FR-008), so this only ever removes
    /// same-group edges; a foreign `RelatesToNode_` loses its hop when the purged-group `Entity`
    /// it pointed to is `DETACH DELETE`d, but the node itself is never matched here.
    pub fn delete_relates_to_by_group_ids(&self, group_ids: &[&str]) -> Result<(), Error> {
        self.exec_params(
            "MATCH (rn:RelatesToNode_) WHERE rn.group_id IN $gids DETACH DELETE rn",
            serde_json::json!({ "gids": group_ids }),
        )
    }

    /// Returns all Entity nodes in the given group_ids, or every group when `group_ids` is
    /// `None`. `Some(&[])` is a real, non-`None` filter and matches no groups.
    pub fn get_entities_by_group_ids(
        &self,
        group_ids: Option<&[&str]>,
    ) -> Result<Vec<EntityRow>, Error> {
        let (cypher, params) = match group_ids {
            Some(gids) => (
                "MATCH (e:Entity) WHERE e.group_id IN $gids \
                 RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes",
                serde_json::json!({ "gids": gids }),
            ),
            None => (
                "MATCH (e:Entity) \
                 RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
                 e.summary, e.attributes",
                serde_json::json!({}),
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

    /// Returns all RELATES_TO edges in the given group_ids, or every group when `group_ids` is
    /// `None`. `Some(&[])` is a real, non-`None` filter and matches no groups.
    pub fn get_edges_by_group_ids(
        &self,
        group_ids: Option<&[&str]>,
    ) -> Result<Vec<RelatesToEdge>, Error> {
        let (cypher, params) = match group_ids {
            Some(gids) => (
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 WHERE rn.group_id IN $gids \
                 RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                 rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
                serde_json::json!({ "gids": gids }),
            ),
            None => (
                "MATCH (src:Entity)-[:RELATES_TO]->(rn:RelatesToNode_)-[:RELATES_TO]->(dst:Entity) \
                 RETURN rn.uuid, rn.name, src.uuid, dst.uuid, rn.group_id, rn.fact, \
                 rn.valid_at, rn.invalid_at, rn.attributes, rn.relation_type",
                serde_json::json!({}),
            ),
        };
        self.collect_relates_to_edges(cypher, params)
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
    /// `group_ids: None` searches across every group; `Some(&[])` is a real filter and matches
    /// no groups.
    pub fn fts_search_entities(
        &self,
        query: &str,
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_filter = match group_ids {
            Some(_) => "WHERE node.group_id IN $gids",
            None => "",
        };
        let cypher = format!(
            "CALL QUERY_FTS_INDEX('Entity', 'node_name_and_summary', $q) \
             WITH node, score {gid_filter} \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "q": query, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            params["gids"] = serde_json::json!(gids);
        }
        self.collect_uuid_score_pairs(&cypher, params)
    }

    /// BM25 full-text search on RelatesToNode_ (facts); returns (uuid, score) pairs.
    /// `group_ids: None` searches across every group; `Some(&[])` is a real filter and matches
    /// no groups.
    pub fn fts_search_edges(
        &self,
        query: &str,
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_filter = match group_ids {
            Some(_) => "WHERE node.group_id IN $gids",
            None => "",
        };
        let cypher = format!(
            "CALL QUERY_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', $q) \
             WITH node, score {gid_filter} \
             RETURN node.uuid, score \
             ORDER BY score DESC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "q": query, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            params["gids"] = serde_json::json!(gids);
        }
        self.collect_uuid_score_pairs(&cypher, params)
    }

    /// HNSW vector search on Entity nodes; returns (uuid, distance) pairs (lower = closer).
    /// `group_ids: None` searches across every group; `Some(&[])` is a real filter and matches
    /// no groups.
    pub fn vector_search_entities(
        &self,
        embedding: &[f32],
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_filter = match group_ids {
            Some(_) => "WHERE node.group_id IN $gids",
            None => "",
        };
        let cypher = format!(
            "CALL QUERY_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', $emb, $limit) \
             WITH node, distance {gid_filter} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "emb": embedding, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            params["gids"] = serde_json::json!(gids);
        }
        self.collect_uuid_score_pairs(&cypher, params)
    }

    /// HNSW vector search on Entity.summary_embedding (issue #470); returns (uuid, distance)
    /// pairs (lower = closer). Mirrors `vector_search_entities` exactly, querying
    /// `entity_summary_embedding_idx` instead of `entity_name_embedding_idx` — this is the query
    /// side of meaning-based retrieval against an entity's `summary`, fused into
    /// `hybrid_entity_search`'s RRF alongside the existing name-vector and FTS lists.
    /// `group_ids: None` searches across every group; `Some(&[])` is a real filter and matches
    /// no groups.
    pub fn vector_search_entities_by_summary(
        &self,
        embedding: &[f32],
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_filter = match group_ids {
            Some(_) => "WHERE node.group_id IN $gids",
            None => "",
        };
        let cypher = format!(
            "CALL QUERY_VECTOR_INDEX('Entity', 'entity_summary_embedding_idx', $emb, $limit) \
             WITH node, distance {gid_filter} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "emb": embedding, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            params["gids"] = serde_json::json!(gids);
        }
        self.collect_uuid_score_pairs(&cypher, params)
    }

    /// HNSW vector search on RelatesToNode_ (facts); returns (uuid, distance) pairs.
    /// `group_ids: None` searches across every group; `Some(&[])` is a real filter and matches
    /// no groups.
    pub fn vector_search_edges(
        &self,
        embedding: &[f32],
        group_ids: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, Error> {
        let gid_filter = match group_ids {
            Some(_) => "WHERE node.group_id IN $gids",
            None => "",
        };
        let cypher = format!(
            "CALL QUERY_VECTOR_INDEX('RelatesToNode_', 'edge_fact_embedding_idx', \
             $emb, $limit) \
             WITH node, distance {gid_filter} \
             RETURN node.uuid, distance \
             ORDER BY distance ASC LIMIT $limit"
        );
        let mut params = serde_json::json!({ "emb": embedding, "limit": limit as i64 });
        if let Some(gids) = group_ids {
            params["gids"] = serde_json::json!(gids);
        }
        self.collect_uuid_score_pairs(&cypher, params)
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
                            ..Default::default()
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
            self.vector_search_entities(name_embedding, Some(&[group_id]), CANDIDATE_K)?;
        let bm25_candidates =
            self.fts_search_entities(entity_name, Some(&[group_id]), CANDIDATE_K)?;
        let fused_uuids = crate::search::rrf_fuse(&[&bm25_candidates, &vector_candidates]);

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
    /// Resolved via the in-process `NameIndex` accelerator rather than a database query
    /// (issue #219 — `lower(e.name) = $x` is a scalar-function predicate lbug cannot route
    /// through any index, so this used to be a full `Entity` table scan on every call). Each
    /// same-named candidate is re-verified against the database via `get_entity_by_uuid`
    /// before being returned (FR-006); if the winning candidate is stale — deleted, or no
    /// longer matching this `name`/`group_id` — the remaining candidates for this key are
    /// tried in turn (FR-005) rather than giving up on the first failure. Returns `Ok(None)`
    /// only once every candidate the index knows of has failed verification (or it knows of
    /// none).
    ///
    /// There is deliberately no scan fallback on a total miss here — see
    /// `get_entity_by_name_ci_with_scan_fallback` for the endpoint-authority call sites that
    /// need one, and the NameIndex ADR / issue #283 for why the two are kept separate.
    pub fn get_entity_by_name_ci(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Option<EntityRow>, Error> {
        let lower_name = name.trim().to_lowercase();
        for uuid in self.name_index.lookup_candidates(name, group_id) {
            if let Some(row) = self.get_entity_by_uuid(&uuid)? {
                if row.group_id == group_id && row.name.trim().to_lowercase() == lower_name {
                    return Ok(Some(row));
                }
            }
        }
        Ok(None)
    }

    /// Case-insensitive, whitespace-normalised name lookup for the endpoint-authority call
    /// sites (issue #283 / #218): unlike `get_entity_by_name_ci`, a miss here does not mean
    /// "the entity doesn't exist" — it may mean the `NameIndex` simply hasn't observed it
    /// (raw Cypher writes, WAL replay whose index rebuild failed, a second writer process).
    /// Those call sites treat "does this entity exist anywhere in the group" as an authority
    /// question, so a miss falls back to a bounded, single-row database scan rather than
    /// trusting the index alone.
    ///
    /// On a scan hit, the result is inserted back into the `NameIndex` (self-healing it for
    /// subsequent lookups in this and later requests). Every fallback scan is counted via
    /// `NameIndex::record_fallback_scan` regardless of outcome (SC-004), so index desync is
    /// observable through `knowledge_status` without reproducing the bug.
    ///
    /// Callers MUST use this sparingly (FR-002): it is intended for a batch's deduplicated
    /// set of unresolved names (e.g. `episode.rs`'s `missing_names`), not per-edge/per-entity
    /// use, or it reintroduces the full-scan cost ADR-0038 removed.
    pub fn get_entity_by_name_ci_with_scan_fallback(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Option<EntityRow>, Error> {
        if let Some(row) = self.get_entity_by_name_ci(name, group_id)? {
            return Ok(Some(row));
        }
        self.name_index.record_fallback_scan();
        let scanned = self.scan_entity_by_name_ci(name, group_id)?;
        if let Some(ref row) = scanned {
            self.name_index
                .insert(&row.uuid, &row.name, &row.group_id, &row.created_at);
        }
        Ok(scanned)
    }

    /// Bounded, single-row full scan backing `get_entity_by_name_ci_with_scan_fallback`'s
    /// miss path. Reproduces the index's own winner-selection rule (`ORDER BY created_at
    /// ASC, uuid ASC LIMIT 1`) and, per the resolved spec assumption, does not filter out
    /// `Merged`-labelled tombstones — the index itself resolves through them (see
    /// `corrections::merge_entities`), so a fallback that filtered them would disagree with
    /// the index on what "resolves" means for a merged-away alias.
    fn scan_entity_by_name_ci(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<Option<EntityRow>, Error> {
        let lower_name = name.trim().to_lowercase();
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE lower(e.name) = $lower_name AND e.group_id = $gid \
             RETURN e.uuid, e.name, e.group_id, e.labels, e.created_at, \
             e.name_embedding, e.summary, e.attributes \
             ORDER BY e.created_at ASC, e.uuid ASC LIMIT 1",
            serde_json::json!({ "lower_name": lower_name, "gid": group_id }),
        )?;
        Ok(rows.into_iter().next().map(|row| EntityRow {
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
            ..Default::default()
        }))
    }

    /// Whether the in-process `NameIndex` is currently believed coherent with the database
    /// (FR-003). Surfaced through `knowledge_status` (SC-004).
    pub fn name_index_trusted(&self) -> bool {
        self.name_index.is_trusted()
    }

    /// Total scan-fallback lookups performed against the `NameIndex` since this `Db` was
    /// opened (SC-004). Surfaced through `knowledge_status`.
    pub fn name_index_fallback_scan_count(&self) -> u64 {
        self.name_index.fallback_scan_count()
    }

    /// Marks the `NameIndex` as potentially stale (FR-003), e.g. after a failed
    /// post-replay `rebuild_name_index()` or a raw-Cypher mutation whose follow-up rebuild
    /// failed. Cleared by the next successful `rebuild_name_index()`.
    pub fn mark_name_index_untrusted(&self) {
        self.name_index.mark_untrusted();
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

    /// Case-insensitive count of *active* (non-`Merged`-tombstoned) entities matching `name`
    /// within `group_id` — filters in Rust after the fetch, mirroring how
    /// `corrections::merge_entities` itself excludes `Merged` rows (`corrections.rs:970`),
    /// rather than a Cypher-side label-list predicate. Unlike `count_entities_by_name_ci`
    /// (which counts every row regardless of tombstone status, used by dedup-regression tests
    /// to assert no *extra* row was created), this is what `cross_group::resolve_endpoint`
    /// needs for ambiguity detection: a name shared by a canonical and its own merged-away
    /// aliases must resolve `Bound` to the canonical, not `Ambiguous` (issue #369 User Story 2
    /// AC 4) — counting tombstones as distinct candidates would contradict that.
    pub fn count_active_entities_by_name_ci(
        &self,
        name: &str,
        group_id: &str,
    ) -> Result<usize, Error> {
        let lower_name = name.trim().to_lowercase();
        let rows = self.query_params(
            "MATCH (e:Entity) WHERE lower(e.name) = $lower_name AND e.group_id = $gid \
             RETURN e.labels",
            serde_json::json!({ "lower_name": lower_name, "gid": group_id }),
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| !value_as_str_list(&row[0]).contains(&"Merged".to_string()))
            .count())
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
                ..Default::default()
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
        )?;
        self.name_index.update_created_at(uuid, created_at);
        Ok(())
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

    /// Counts cross-group pointers by `binding_state` across every non-invalidated
    /// `RelatesToNode_` row (FR-012), so a refresh in progress is observable via
    /// `knowledge_status`. Excludes invalidated rows (`rn.invalid_at IS NOT NULL`) — matching
    /// `list_cross_group_pointer_candidates`'s candidate set — since an edge invalidated by
    /// `rebind_pointers`'s self-loop/duplicate handling is no longer a live assertion and
    /// counting it would inflate the reported state and mislead refresh observability. A
    /// `CONTAINS` pre-filter narrows the scan to nodes carrying the `cross_group_pointers` key at
    /// all, followed by a real JSON parse for per-pointer (not per-node) accuracy. This is a
    /// full-table scan, acceptable because it's admin/status-triggered, not on the hot
    /// read/write path — cross-group edges are expected to stay a small minority of total edges.
    pub fn count_cross_group_pointers(&self) -> Result<crate::pointer::PointerStateCounts, Error> {
        let rows = self.query_params(
            "MATCH (rn:RelatesToNode_) \
             WHERE rn.attributes CONTAINS '\"cross_group_pointers\"' AND rn.invalid_at IS NULL \
             RETURN rn.attributes",
            serde_json::json!({}),
        )?;
        let mut counts = crate::pointer::PointerStateCounts::default();
        for row in rows {
            let attrs = value_as_string(&row[0]);
            for (_, ptr) in crate::pointer::read_pointers(&attrs).iter() {
                counts.record(ptr.binding_state);
            }
        }
        Ok(counts)
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

    /// Returns the uuid of the most-recently created Episodic node belonging to `group_id`, or
    /// `None` if that group has no episodes yet. Used by episode-cursor derivation during WAL
    /// recovery, scoped per group (issue #378 FR-010) so a backfill for one group can never pick
    /// up another group's most recent episode.
    pub fn get_latest_episode_uuid(&self, group_id: &str) -> Result<Option<String>, Error> {
        let rows = self.query_params(
            "MATCH (ep:Episodic {group_id: $group_id}) RETURN ep.uuid \
             ORDER BY ep.created_at DESC LIMIT 1",
            serde_json::json!({ "group_id": group_id }),
        )?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| value_as_optional_string(&row[0])))
    }

    /// Returns the persisted `(applied_seq, generation)` position for `group_id`, or the default
    /// (`None`, `None`) if no position has ever been recorded for that group. A `seq`/generation
    /// from one group's stream must never be compared against another group's row (FR-008 of
    /// #378) — this always reads exactly one group's `WalPosition` row, selected by its primary
    /// key.
    pub fn get_wal_position(&self, group_id: &str) -> Result<WalPositionRecord, Error> {
        let rows = self.query_params(
            "MATCH (w:WalPosition {id: $group_id}) RETURN w.applied_seq, w.generation",
            serde_json::json!({ "group_id": group_id }),
        )?;
        Ok(rows
            .into_iter()
            .next()
            .map(|row| WalPositionRecord {
                applied_seq: value_as_optional_u64(&row[0]),
                generation: value_as_optional_string(&row[1]),
            })
            .unwrap_or_default())
    }

    /// Persists `seq` and `generation` as `group_id`'s WAL position (issue #353, made per-group
    /// by issue #378, generation-scoped by issue #387). Uses `Connection::execute` via a prepared
    /// statement directly rather than `raw_query`/`exec_params`, so this write is never itself
    /// recorded into `executed_mutations` and re-logged to the WAL — that would make the position
    /// immediately stale by the write that just recorded it (a self-referential regress). Same
    /// non-recording bypass as `exec_transaction_control`/`count_nodes`. `group_id` is bound as a
    /// parameter, not interpolated, since it is caller-controlled (IPC-supplied).
    ///
    /// `generation: None` is a legitimate value (a legacy pre-#387 stream that has never had a
    /// generation recorded, per FR-009's Story 5) — it is written as an explicit SQL `NULL`, not
    /// skipped, so a later `set_wal_position` call for the same group correctly clears out any
    /// stale generation from a previous write rather than leaving it silently orphaned.
    pub fn set_wal_position(
        &self,
        group_id: &str,
        seq: u64,
        generation: Option<&str>,
    ) -> Result<(), Error> {
        let mut prepared = self.inner.prepare(
            "MERGE (w:WalPosition {id: $group_id}) SET w.applied_seq = $seq, \
             w.generation = $generation",
        )?;
        let bound: Vec<(&str, Value)> = vec![
            ("group_id", Value::String(group_id.to_string())),
            ("seq", Value::Int64(seq as i64)),
            (
                "generation",
                match generation {
                    Some(g) => Value::String(g.to_string()),
                    None => Value::Null(LogicalType::Any),
                },
            ),
        ];
        self.inner.execute(&mut prepared, bound)?;
        Ok(())
    }

    /// One-time migration (issue #378 FR-001/FR-009): carries a pre-378 database's
    /// `WalPosition {id: 'singleton'}` row forward to `group_id`'s own row. Before this issue,
    /// `get_applied_seq`/`set_applied_seq` hardcoded `'singleton'` as the row's primary key;
    /// they now key on `group_id` instead, so an upgraded binary's `get_applied_seq(group_id)`
    /// would otherwise find no row at all for a deployment that had a durably-recorded position
    /// under the old key — silently degrading a known position to `None` ("unknown") and forcing
    /// a WAL re-scan (`backfill_applied_seq_if_absent`) to reconstruct it, contradicting FR-009's
    /// "behaves exactly as it does in 0.12.2" / "no operator action required" guarantee. Must run
    /// before `backfill_applied_seq_if_absent` is given the chance to decide a backfill is
    /// needed, or the legacy value is never consulted.
    ///
    /// No-op if no legacy row exists (fresh install, or a second boot after the first migration
    /// already ran — idempotent). Never overwrites an already-present `group_id` row: if one
    /// exists, the singleton row is stale leftover from before `group_id`'s own value was
    /// established and is simply removed, not blended with it — `group_id`'s existing row is by
    /// construction more recent than a not-yet-migrated legacy row could be.
    ///
    /// The legacy singleton row predates generation entirely (issue #387), so the carried-forward
    /// value has no generation to bring with it — it lands with `generation: None`, the same
    /// "unknown" state FR-009 already defines for any pre-#387 recorded position.
    pub fn migrate_legacy_singleton_wal_position(&self, group_id: &str) -> Result<(), Error> {
        let legacy = self
            .inner
            .query("MATCH (w:WalPosition {id: 'singleton'}) RETURN w.applied_seq")?;
        let Some(seq) = legacy
            .into_iter()
            .next()
            .and_then(|row| value_as_optional_u64(&row[0]))
        else {
            return Ok(());
        };
        if self.get_wal_position(group_id)?.applied_seq.is_none() {
            self.set_wal_position(group_id, seq, None)?;
        }
        let _ = self
            .inner
            .query("MATCH (w:WalPosition {id: 'singleton'}) DETACH DELETE w")?;
        Ok(())
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
    /// `source_uuid` to `target_uuid`, scoped to `group_id`. The name filter prevents
    /// over-deduplication when the canonical entity has semantically different relationships
    /// to the same target. The `group_id` filter prevents a different group's edge from being
    /// mistaken for a duplicate of the caller's own (see issue #368).
    pub fn has_directed_edge(
        &self,
        source_uuid: &str,
        target_uuid: &str,
        name: &str,
        group_id: &str,
    ) -> Result<bool, Error> {
        Ok(self
            .find_active_relates_to_uuid(source_uuid, target_uuid, name, group_id)?
            .is_some())
    }

    /// Returns the `RelatesToNode_.uuid` of the active (non-invalidated) directed `RELATES_TO`
    /// edge with the given `name` from `source_uuid` to `target_uuid`, scoped to `group_id` —
    /// the same match `has_directed_edge` checks for existence, but returning the matched edge's
    /// UUID rather than a boolean, so a caller can update that edge in place (issue #379
    /// FR-016/FR-017's edge-upsert path). `has_directed_edge` is defined in terms of this
    /// function so there is exactly one implementation of the group-scoped match (ADR-0368).
    pub fn find_active_relates_to_uuid(
        &self,
        source_uuid: &str,
        target_uuid: &str,
        name: &str,
        group_id: &str,
    ) -> Result<Option<String>, Error> {
        let rows = self.query_params(
            "MATCH (src:Entity {uuid: $src})-[:RELATES_TO]->(rn:RelatesToNode_ {name: $name})-[:RELATES_TO]->(dst:Entity {uuid: $dst}) \
             WHERE rn.invalid_at IS NULL AND rn.group_id = $group_id \
             RETURN rn.uuid ORDER BY rn.created_at ASC, rn.uuid ASC LIMIT 1",
            serde_json::json!({ "src": source_uuid, "name": name, "dst": target_uuid, "group_id": group_id }),
        )?;
        Ok(rows.into_iter().next().map(|row| value_as_string(&row[0])))
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

    /// Updates the `attributes` JSON column on the Entity with the given UUID (mirrors
    /// `update_relates_to_attributes` for the edge side). Used by `corrections::merge_entities`
    /// to record a `merged_into` forwarding reference on a tombstoned alias (issue #371).
    pub fn update_entity_attributes(&self, uuid: &str, attributes: &str) -> Result<(), Error> {
        self.exec_params(
            "MATCH (e:Entity {uuid: $uuid}) SET e.attributes = $attributes",
            serde_json::json!({ "uuid": uuid, "attributes": attributes }),
        )
    }

    /// Updates an existing Entity's mutable fields — `name`, `labels` (through
    /// `enforce_entity_first`, since this path doesn't go through `insert_entity`'s own
    /// invariant enforcement), `summary`, and `attributes` — in a single `SET` statement, then
    /// refreshes the `NameIndex` entry for `existing.uuid` under the (possibly changed)
    /// `new_name`. Used by `knowledge_assert_entity`'s update-in-place path (issue #379
    /// FR-011): no existing narrow setter (`update_entity_labels`/`update_entity_attributes`)
    /// covers `name`/`summary` in one shot.
    ///
    /// Deliberately does **not** touch `name_embedding`, even though FR-012 asks for a fresh
    /// name embedding on every assert call. lbug's HNSW vector index (built over `Entity.
    /// name_embedding` by `create_vector_indexes`) rejects a plain `SET` on an indexed column
    /// outright: `Cannot set property name_embedding in table Entity because it is used in one
    /// or more indexes. Try delete and then insert.` This is the same reason
    /// `episode.rs`'s dedup `DedupDecision::Merge` path (`SET e.summary = $summary`) never
    /// rewrites `name_embedding` on a re-matched entity either — it is this codebase's existing
    /// precedent, not a new decision invented for this feature. The caller still generates the
    /// embedding on every call (so the embedder-unavailable `embedding_warning` fallback stays
    /// observable and consistent between create and update), but only the create path
    /// (`insert_entity`, before any index exists over the row) actually persists it; an update
    /// leaves the entity's previously-stored embedding untouched.
    pub fn update_entity_core(
        &self,
        existing: &EntityRow,
        new_name: &str,
        labels: &[String],
        summary: &str,
        attributes: &str,
    ) -> Result<(), Error> {
        let labels = enforce_entity_first(labels);
        self.exec_params(
            "MATCH (e:Entity {uuid: $uuid}) SET e.name = $name, e.labels = $labels, \
             e.summary = $summary, e.attributes = $attributes",
            serde_json::json!({
                "uuid": existing.uuid,
                "name": new_name,
                "labels": labels,
                "summary": summary,
                "attributes": attributes,
            }),
        )?;
        self.name_index.insert(
            &existing.uuid,
            new_name,
            &existing.group_id,
            &existing.created_at,
        );
        Ok(())
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
/// | `insert_cross_group_edge` (#369)        | `exec_params`    | ✓ exec_params gate          |
/// | `create_relates_to_hop` (#369)          | `exec_params`    | ✓ no timestamp fields       |
/// | `delete_relates_to_hop` (#369)          | `exec_params`    | ✓ no timestamp fields       |
/// | `update_relates_to_attributes` (#369)   | `exec_params`    | ✓ no timestamp fields       |
/// | `create_relates_to_direct` (#369)       | `exec_params`    | ✓ exec_params gate          |
/// | `delete_relates_to_direct` (#369)       | `exec_params`    | ✓ no timestamp fields       |
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
            if let Some(odt) = parse_timestamp_str(s) {
                return Value::Timestamp(odt);
            }
        }
    }
    json_to_value(v)
}

/// Parses a timestamp string in either format this codebase accepts on a `TIMESTAMP` column:
/// RFC-3339 (the WAL write-path format) or lbug's space-delimited `"YYYY-MM-DD HH:MM:SS"`
/// read-back format (assumed UTC). Shared by `json_value_for_param`'s column-write coercion and
/// [`validate_and_normalize_valid_at`]'s upfront call-time validation, so there is exactly one
/// definition of "which formats we accept" rather than a second copy that could drift.
fn parse_timestamp_str(s: &str) -> Option<time::OffsetDateTime> {
    if let Ok(odt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
    {
        return Some(odt);
    }
    const SPACE_FMT: &[time::format_description::FormatItem<'static>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    if let Ok(pdt) = time::PrimitiveDateTime::parse(s, SPACE_FMT) {
        return Some(pdt.assume_utc());
    }
    None
}

/// Validates and normalizes a caller-supplied `valid_at` timestamp (RFC-3339 or lbug's
/// space-delimited read-back format) to canonical RFC-3339 *before* any `exec_params` call —
/// closing a real lbug `Binder exception` hazard on unparseable input at the point the caller's
/// value is first seen, rather than inside a write closure after other statements in the same
/// handler may already have executed (issue #379 FR-022).
pub fn validate_and_normalize_valid_at(raw: &str) -> Result<String, Error> {
    parse_timestamp_str(raw)
        .map(|odt| {
            odt.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| raw.to_string())
        })
        .ok_or_else(|| {
            Error::Ipc(format!(
                "valid_at '{raw}' is not a valid RFC-3339 or 'YYYY-MM-DD HH:MM:SS' timestamp"
            ))
        })
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

/// Reads an `INT64` column as `Option<u64>`, treating `Null` as `None` rather than `0` — the
/// `applied_seq` "unknown" state (issue #353) must stay distinguishable from "nothing applied".
/// A negative value (never written by `set_wal_position`, but not excluded by the `INT64` column
/// type — e.g. a hand-edited or corrupted row) is also treated as `None` rather than wrapping to
/// a huge `u64` via `as` casting, which would otherwise report a nonsensical applied position.
fn value_as_optional_u64(v: &lbug::Value) -> Option<u64> {
    match v {
        lbug::Value::Int64(i) if *i >= 0 => Some(*i as u64),
        lbug::Value::UInt64(i) => Some(*i),
        lbug::Value::Int32(i) if *i >= 0 => Some(*i as u64),
        _ => None,
    }
}

/// Reads a `RETURN count(*)` probe result as `i64`. See
/// `Conn::execute_prepared_returning_count` for the "unexpected shape ⇒ treat as matched"
/// fallback rationale.
fn value_as_match_count(v: &lbug::Value) -> i64 {
    match v {
        lbug::Value::Int64(i) => *i,
        lbug::Value::UInt64(i) => *i as i64,
        lbug::Value::Int32(i) => *i as i64,
        lbug::Value::Double(f) => *f as i64,
        _ => 1,
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

/// Normalizes a `created_at` string (RFC-3339 or space-format) to the canonical
/// `"YYYY-MM-DD HH:MM:SS"` space form used by `value_as_timestamp_str`/`format_datetime`.
///
/// `NameIndex` (issue #219) sorts entries lexicographically on this string to reproduce the
/// database's `ORDER BY created_at ASC` winner-selection rule, which only holds if every entry
/// sharing a key is in the same format — freshly-inserted rows arrive via `insert_entity` as
/// whatever the caller passed (typically RFC-3339, e.g. episode.rs's `reference_time`), while
/// `rebuild_name_index()` always produces the space form read back from the database. Without
/// normalization the `'T'`/`' '` separator byte dominates the comparison ahead of the actual
/// time-of-day, silently picking the wrong deterministic winner. Falls through verbatim if
/// neither format parses (defensive; should not happen for a valid `created_at`).
pub(crate) fn canonical_created_at_for_index(s: &str) -> String {
    if let Ok(odt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
    {
        return format_datetime(odt);
    }
    const SPACE_FMT: &[time::format_description::FormatItem<'static>] =
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    if let Ok(pdt) = time::PrimitiveDateTime::parse(s, SPACE_FMT) {
        return format_datetime(pdt.assume_utc());
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
            .fts_search_entities("probe", Some(&["g"]), 5)
            .expect_err("should fail with missing FTS index");
        let msg = err.to_string();
        assert!(
            msg.contains("Binder exception:") && msg.contains("doesn't have an index with name"),
            "FTS missing-index error must match the same pattern as HNSW — got: {msg}"
        );
    }
}

#[cfg(test)]
mod missing_table_error_tests {
    use super::*;
    use tempfile::TempDir;

    /// Regression guard for issue #325: `count_nodes` against a label whose table doesn't exist
    /// (schema never initialized) must classify as `is_missing_table_error`, and must not be
    /// misclassified as either of the other two `Binder exception:` classifiers.
    #[test]
    fn missing_table_error_matches_binder_exception() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("missing_table_probe.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        // No init_schema() — the Entity table doesn't exist.
        let err = conn
            .count_nodes("Entity")
            .expect_err("should fail with missing table");
        let msg = err.to_string();
        assert!(
            msg.contains("Binder exception:") && msg.contains("does not exist"),
            "missing-table error must match the expected binder exception pattern — got: {msg}"
        );
        assert!(
            crate::error::is_missing_table_error(&err),
            "is_missing_table_error must classify a genuine missing-table error: {msg}"
        );
        assert!(
            !crate::error::is_missing_index_error(&err),
            "missing-table error must not be misclassified as missing-index: {msg}"
        );
        assert!(
            !crate::error::is_already_exists_error(&err),
            "missing-table error must not be misclassified as already-exists: {msg}"
        );
    }

    /// The other two `Binder exception:` classifiers must not misclassify their own errors as
    /// missing-table, keeping all three variants textually disjoint.
    #[test]
    fn missing_index_error_is_not_misclassified_as_missing_table() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("fts_vs_table_probe.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        crate::schema::drop_fts_indexes(&conn);
        conn.insert_entity(&crate::EntityRow {
            uuid: "missing-table-probe-1".to_string(),
            name: "MissingTableProbeEntity".to_string(),
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
            .fts_search_entities("probe", Some(&["g"]), 5)
            .expect_err("should fail with missing FTS index");
        assert!(
            !crate::error::is_missing_table_error(&err),
            "missing-index error must not be misclassified as missing-table: {err}"
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

#[cfg(test)]
mod exec_transaction_control_tests {
    use super::*;
    use tempfile::TempDir;

    /// `exec_transaction_control` must not accumulate into `executed_mutations` — that buffer is
    /// drained for live-write WAL logging, and a replay connection's transaction-control calls
    /// (BEGIN/COMMIT/ROLLBACK, one pair per flushed batch) must not grow it unboundedly over a
    /// multi-hour replay (issue #240, User Story 4).
    #[test]
    fn does_not_accumulate_in_executed_mutations() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        conn.drain_mutations(); // discard init_schema's own recorded DDL

        for _ in 0..5 {
            conn.exec_transaction_control("BEGIN TRANSACTION").unwrap();
            conn.exec_transaction_control("COMMIT").unwrap();
        }

        assert!(
            conn.drain_mutations().is_empty(),
            "exec_transaction_control must never record into executed_mutations"
        );
    }

    /// A statement executed inside the open transaction still records normally via
    /// `exec_params`/`raw_query` — only the transaction-control calls themselves are exempt.
    #[test]
    fn does_not_suppress_recording_of_statements_inside_the_transaction() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        conn.drain_mutations(); // discard init_schema's own recorded DDL

        conn.exec_transaction_control("BEGIN TRANSACTION").unwrap();
        conn.raw_query("CREATE (:Episodic {uuid: 'txn-record-probe', name: 'n', group_id: 'g', created_at: timestamp('2026-01-01'), source: 'text', source_description: '', content: 'c', valid_at: timestamp('2026-01-01')})").unwrap();
        conn.exec_transaction_control("COMMIT").unwrap();

        assert_eq!(
            conn.drain_mutations().len(),
            1,
            "the one statement executed between BEGIN and COMMIT must still be recorded"
        );
    }
}

#[cfg(test)]
mod validate_and_normalize_valid_at_tests {
    use super::*;

    /// RFC-3339 input round-trips to RFC-3339 (issue #379 FR-022).
    #[test]
    fn accepts_rfc3339() {
        let out = validate_and_normalize_valid_at("2026-06-24T10:00:00Z").unwrap();
        assert!(out.starts_with("2026-06-24T10:00:00"));
    }

    /// lbug's space-delimited read-back format is also accepted and normalized to RFC-3339.
    #[test]
    fn accepts_space_format() {
        let out = validate_and_normalize_valid_at("2026-06-24 10:00:00").unwrap();
        assert!(
            out.starts_with("2026-06-24T10:00:00"),
            "expected RFC-3339 output, got {out}"
        );
    }

    /// An unparseable value must fail cleanly (issue #379 FR-022's Binder-exception hazard),
    /// not be passed through to `exec_params` unvalidated.
    #[test]
    fn rejects_unparseable_value() {
        let err = validate_and_normalize_valid_at("not-a-timestamp")
            .expect_err("must reject an unparseable valid_at");
        assert!(matches!(err, Error::Ipc(_)));
    }
}

#[cfg(test)]
mod applied_seq_tests {
    use super::*;
    use tempfile::TempDir;

    /// Absent row (fresh DB, never written) must read back as `None`, not `0` — the "unknown"
    /// state (issue #353, FR-008) must stay distinguishable from "nothing applied".
    #[test]
    fn get_applied_seq_is_none_when_row_absent() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        let pos = conn.get_wal_position("liminis").unwrap();
        assert_eq!(pos.applied_seq, None);
        assert_eq!(pos.generation, None);
    }

    /// Basic write/read round-trip.
    #[test]
    fn set_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("liminis", 41, None).unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(41)
        );
    }

    /// `set_wal_position` MERGEs onto that group's row rather than inserting a duplicate —
    /// repeated writes (the normal case, once per chunk) must overwrite, not accumulate.
    #[test]
    fn set_applied_seq_overwrites_existing_value() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("liminis", 5, None).unwrap();
        conn.set_wal_position("liminis", 12, None).unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(12)
        );
        assert_eq!(
            conn.count_nodes("WalPosition").unwrap(),
            1,
            "MERGE must not create a second WalPosition row"
        );
    }

    /// An explicit reset to `0` (FR-005: `knowledge_clear_all` / fresh rebuild) must read back
    /// as `Some(0)`, distinct from row-absence (`None`) — "known, nothing applied" vs. "unknown".
    #[test]
    fn set_applied_seq_zero_is_distinct_from_absent() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("liminis", 0, None).unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(0)
        );
    }

    /// `get_wal_position`/`set_wal_position` must not themselves become a WAL line — using
    /// `raw_query`/`exec_params` here would make the position immediately stale by the write
    /// that just recorded it.
    #[test]
    fn set_applied_seq_does_not_record_into_executed_mutations() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
        conn.drain_mutations(); // discard init_schema's own recorded DDL

        conn.set_wal_position("liminis", 7, None).unwrap();

        assert!(
            conn.drain_mutations().is_empty(),
            "set_wal_position must never record into executed_mutations"
        );
    }

    /// Two different `group_id`s must produce two independent `WalPosition` rows: writing one
    /// group's position must never be visible from, or overwrite, another group's row (issue
    /// #378 FR-002/FR-008 — SC-003 groundwork).
    #[test]
    fn different_groups_have_independent_applied_seq_rows() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("group-a", 10, None).unwrap();
        conn.set_wal_position("group-b", 0, None).unwrap();

        assert_eq!(
            conn.get_wal_position("group-a").unwrap().applied_seq,
            Some(10)
        );
        assert_eq!(
            conn.get_wal_position("group-b").unwrap().applied_seq,
            Some(0),
            "group-b's own Some(0) must not be shadowed by group-a's higher seq"
        );
        assert_eq!(
            conn.get_wal_position("group-c").unwrap().applied_seq,
            None,
            "a third, never-written group must still report unknown, not either sibling's value"
        );
        assert_eq!(
            conn.count_nodes("WalPosition").unwrap(),
            2,
            "each group must own exactly one WalPosition row"
        );

        // Advancing group-a must not disturb group-b's row (SC-002/SC-003 groundwork).
        conn.set_wal_position("group-a", 25, None).unwrap();
        assert_eq!(
            conn.get_wal_position("group-b").unwrap().applied_seq,
            Some(0)
        );
    }

    /// issue #387 (FR-004): the generation persisted alongside `applied_seq` must round-trip
    /// exactly, and a later write with a different generation must overwrite the prior one — the
    /// same single-row MERGE semantics `applied_seq` itself already has, extended to the new
    /// column.
    #[test]
    fn generation_round_trips_and_overwrites_with_applied_seq() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("liminis", 5, Some("gen-a")).unwrap();
        let pos = conn.get_wal_position("liminis").unwrap();
        assert_eq!(pos.applied_seq, Some(5));
        assert_eq!(pos.generation.as_deref(), Some("gen-a"));

        conn.set_wal_position("liminis", 9, Some("gen-b")).unwrap();
        let pos = conn.get_wal_position("liminis").unwrap();
        assert_eq!(pos.applied_seq, Some(9));
        assert_eq!(
            pos.generation.as_deref(),
            Some("gen-b"),
            "a later write must overwrite the prior generation, not accumulate or ignore it"
        );
    }

    /// Writing `generation: None` over a row that previously had `Some` must clear it, not leave
    /// the prior value orphaned — `set_wal_position` always writes both fields together.
    #[test]
    fn generation_none_write_clears_a_previously_recorded_value() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("liminis", 5, Some("gen-a")).unwrap();
        conn.set_wal_position("liminis", 6, None).unwrap();

        let pos = conn.get_wal_position("liminis").unwrap();
        assert_eq!(pos.applied_seq, Some(6));
        assert_eq!(pos.generation, None);
    }

    /// Two different groups' generations must be independent, matching `applied_seq`'s existing
    /// per-group isolation (SC-008 groundwork).
    #[test]
    fn different_groups_have_independent_generations() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("group-a", 10, Some("gen-a")).unwrap();
        conn.set_wal_position("group-b", 10, Some("gen-b")).unwrap();

        assert_eq!(
            conn.get_wal_position("group-a")
                .unwrap()
                .generation
                .as_deref(),
            Some("gen-a")
        );
        assert_eq!(
            conn.get_wal_position("group-b")
                .unwrap()
                .generation
                .as_deref(),
            Some("gen-b")
        );
    }

    /// A genuine pre-378 database's `WalPosition {id: 'singleton'}` row (simulated here by
    /// writing under the literal "singleton" id, exactly as pre-378 `set_applied_seq` did) must
    /// be carried forward to the default group's own row, and the legacy row removed — otherwise
    /// an upgraded binary's `get_wal_position("liminis")` finds nothing and a known position
    /// silently degrades to "unknown" (issue #378 FR-001/FR-009).
    #[test]
    fn migrate_legacy_singleton_wal_position_carries_value_forward_and_removes_legacy_row() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("singleton", 41, None).unwrap();

        conn.migrate_legacy_singleton_wal_position("liminis")
            .unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(41),
            "the legacy position must be visible under the default group's own id post-migration"
        );
        assert_eq!(
            conn.get_wal_position("singleton").unwrap().applied_seq,
            None,
            "the legacy row itself must be gone, not merely superseded"
        );
        assert_eq!(
            conn.count_nodes("WalPosition").unwrap(),
            1,
            "migration must not leave both the old and new rows behind"
        );
    }

    /// A fresh install (no legacy row) is a no-op — nothing to carry forward, and no spurious
    /// `liminis` row is created where none was requested.
    #[test]
    fn migrate_legacy_singleton_wal_position_is_noop_when_no_legacy_row() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.migrate_legacy_singleton_wal_position("liminis")
            .unwrap();

        assert_eq!(conn.get_wal_position("liminis").unwrap().applied_seq, None);
        assert_eq!(conn.count_nodes("WalPosition").unwrap(), 0);
    }

    /// If the default group's own row already exists (e.g. a write already landed under the new
    /// key before migration ran), the legacy row must not overwrite it — it is simply discarded
    /// as stale leftover, never blended with or preferred over the current value.
    #[test]
    fn migrate_legacy_singleton_wal_position_does_not_overwrite_existing_group_row() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("singleton", 5, None).unwrap();
        conn.set_wal_position("liminis", 99, None).unwrap();

        conn.migrate_legacy_singleton_wal_position("liminis")
            .unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(99),
            "an already-present group row must win over the stale legacy value"
        );
        assert_eq!(
            conn.get_wal_position("singleton").unwrap().applied_seq,
            None
        );
    }

    /// A second call (e.g. a subsequent boot) after the legacy row has already been migrated
    /// away must be a harmless no-op, not an error.
    #[test]
    fn migrate_legacy_singleton_wal_position_is_idempotent_on_second_call() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();

        conn.set_wal_position("singleton", 7, None).unwrap();
        conn.migrate_legacy_singleton_wal_position("liminis")
            .unwrap();
        conn.migrate_legacy_singleton_wal_position("liminis")
            .unwrap();

        assert_eq!(
            conn.get_wal_position("liminis").unwrap().applied_seq,
            Some(7)
        );
        assert_eq!(conn.count_nodes("WalPosition").unwrap(), 1);
    }
}

/// Pins lbug 0.17.0 engine behavior (verified against the vendored C++ source, not documented in
/// the Rust crate's own API surface or exercised by its own test suite — see issue #240's Research
/// findings) that the replay transaction-boundary design in `replay.rs::flush_batch` depends on.
/// If a future lbug version changes either behavior pinned here, these tests must fail loudly
/// rather than let `flush_batch`'s assumptions silently go stale.
#[cfg(test)]
mod lbug_transaction_semantics_pinning_tests {
    use super::*;
    use tempfile::TempDir;

    fn open_db_with_schema() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.init_schema(4).unwrap();
        }
        (dir, db)
    }

    /// A statement that throws an execute-time exception inside an open explicit transaction
    /// rolls back EVERY statement already applied earlier in that same transaction — not just
    /// the failing one — and the engine has already cleared its transaction state by the time
    /// the exception is caught, so a subsequent explicit `ROLLBACK` then itself errors ("no
    /// active transaction"). This is the constraint `flush_batch` is built around: once `BEGIN`
    /// has been issued, a per-row execute failure must NOT be followed by an explicit `ROLLBACK`
    /// call.
    #[test]
    fn execute_exception_rolls_back_whole_transaction_and_leaves_no_active_transaction() {
        let (_dir, db) = open_db_with_schema();
        let conn = db.connect().unwrap();

        conn.insert_entity(&crate::EntityRow {
            uuid: "pin-rollback-target".to_string(),
            name: "N".to_string(),
            group_id: "g".to_string(),
            labels: vec![],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0f32; 4],
            summary: "initial".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();

        conn.exec_transaction_control("BEGIN TRANSACTION").unwrap();

        // A statement that succeeds earlier in the transaction.
        conn.raw_query(
            "CREATE (:Episodic {uuid: 'pin-rollback-episode', name: 'n', group_id: 'g', \
             created_at: timestamp('2026-01-01'), source: 'text', source_description: '', \
             content: 'c', valid_at: timestamp('2026-01-01')})",
        )
        .unwrap();

        // "bad_val" is not in TIMESTAMP_PARAM_NAMES, so it binds as a plain string; assigning it
        // into the TIMESTAMP `created_at` column fails at execute() — a genuine exception, not a
        // zero-row no-op.
        let exec_err = conn
            .exec_params(
                "MATCH (n:Entity {uuid: $uuid}) SET n.created_at = $bad_val",
                serde_json::json!({"uuid": "pin-rollback-target", "bad_val": "not-a-real-timestamp"}),
            )
            .expect_err("type-mismatched SET must fail at execute time");
        eprintln!("(expected) execute exception: {exec_err}");

        // The engine has already rolled back and cleared its transaction state — an explicit
        // ROLLBACK at this point must itself error.
        let rollback_err = conn
            .exec_transaction_control("ROLLBACK")
            .expect_err("ROLLBACK after an engine auto-rollback must error");
        assert!(
            rollback_err
                .to_string()
                .to_lowercase()
                .contains("active transaction"),
            "expected a 'no active transaction' style error, got: {rollback_err}"
        );

        // The earlier-successful CREATE inside the same transaction must also have been rolled
        // back — not just the failing SET.
        let rows = conn
            .query_params(
                "MATCH (ep:Episodic {uuid: $uuid}) RETURN ep.uuid",
                serde_json::json!({"uuid": "pin-rollback-episode"}),
            )
            .unwrap();
        assert!(
            rows.is_empty(),
            "the earlier CREATE in the same transaction must have been rolled back too"
        );
    }

    /// A `PreparedStatement` prepared once outside any transaction continues to execute
    /// correctly when invoked inside two separate, later `BEGIN`/`COMMIT` transactions — this is
    /// what makes `flush_batch`'s cross-flush `PreparedCache` (issue #238/ADR-0045) safe to keep
    /// using unmodified once each flush wraps its row loop in an explicit transaction.
    #[test]
    fn prepared_statement_reused_safely_across_separate_transactions() {
        let (_dir, db) = open_db_with_schema();
        let conn = db.connect().unwrap();

        let mut prepared = conn
            .prepare(
                "CREATE (:Episodic {uuid: $uuid, name: 'n', group_id: 'g', \
                 created_at: timestamp('2026-01-01'), source: 'text', source_description: '', \
                 content: 'c', valid_at: timestamp('2026-01-01')})",
            )
            .unwrap();

        conn.exec_transaction_control("BEGIN TRANSACTION").unwrap();
        conn.execute_prepared(&mut prepared, &serde_json::json!({"uuid": "cross-txn-a"}))
            .unwrap();
        conn.exec_transaction_control("COMMIT").unwrap();

        conn.exec_transaction_control("BEGIN TRANSACTION").unwrap();
        conn.execute_prepared(&mut prepared, &serde_json::json!({"uuid": "cross-txn-b"}))
            .unwrap();
        conn.exec_transaction_control("COMMIT").unwrap();

        for uuid in ["cross-txn-a", "cross-txn-b"] {
            let rows = conn
                .query_params(
                    "MATCH (ep:Episodic {uuid: $uuid}) RETURN ep.uuid",
                    serde_json::json!({"uuid": uuid}),
                )
                .unwrap();
            assert_eq!(
                rows.len(),
                1,
                "node {uuid} must exist — the reused prepared statement must have executed \
                 correctly in both transactions"
            );
        }
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
