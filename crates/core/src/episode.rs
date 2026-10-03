use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use chrono::DateTime;

use crate::{
    app_state::{build_indices_once, load_db, AppState, OntologyDriftState},
    canonicalize::build_alias_map,
    db::Db,
    dedup_adapter::{pair_from, DedupMode},
    dedup_judge::{DedupVerdict, DuplicatePair},
    error::{is_missing_index_error, Error, MISSING_INDEX_USER_MSG},
    extractor::ExtractOptions,
    ontology::{normalize_entity_type, normalize_relation_type, OntologyMode},
    ontology_sidecar,
    prompts::normalize_name,
    reprocess_relations::UNCLASSIFIED,
    types::{
        DroppedEdgeDetail, EntityRow, EpisodicRow, ExtractionOutcome, MentionsEdge, RelatesToEdge,
        SourceType, UnresolvedEndpoint,
    },
    wal_exec,
};

/// Per-chunk tally of Phase B entity-resolution outcomes by path (issue #650, ADR-0650). One
/// named field per path so later work (#652's LLM-confirmed / LLM-rejected paths) can extend the
/// struct without reshaping call sites. Each extracted entity increments exactly one of
/// `exact_name`, `embedding_merge`, `vetoed`, `adapter_rejected`, `llm_confirmed`, `llm_rejected`, `llm_unavailable`, or none (a plain insert with no
/// above-threshold candidate); `salvage_vetoed` counts off-list edge endpoints, not entities.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DedupPathCounts {
    /// Merged via the exact case-insensitive name match (no veto, no adapter).
    pub exact_name: usize,
    /// Merged on the embedding path without an LLM verdict: a candidate survived the identifier
    /// veto and either shared the incoming entity's normalized name or the (veto-only) adapter
    /// confirmed it.
    pub embedding_merge: usize,
    /// Above-threshold candidates existed but every one was discarded by the identifier-mismatch
    /// veto, so the entity was inserted. Counted once per incoming entity.
    pub vetoed: usize,
    /// A candidate survived the veto but a non-LLM (custom/legacy) dedup adapter said "not a
    /// duplicate", so the entity was inserted.
    pub adapter_rejected: usize,
    /// The LLM dedup check (#652) judged a surviving candidate a duplicate and it merged.
    pub llm_confirmed: usize,
    /// The LLM dedup check judged a surviving candidate distinct, so the entity was inserted.
    pub llm_rejected: usize,
    /// The LLM dedup check could not give a verdict (error, timeout, malformed or unattributable
    /// answer), so the entity was inserted.
    pub llm_unavailable: usize,
    /// Off-list edge endpoints whose above-threshold salvage candidates were all vetoed.
    pub salvage_vetoed: usize,
}

#[derive(Debug)]
pub struct AddEpisodeResult {
    pub episode_uuid: String,
    pub nodes_extracted: usize,
    pub edges_extracted: usize,
    /// Edges whose endpoint(s) could not be resolved against either this batch's entities or
    /// the persisted graph, and were dropped at Phase C commit time (issue #281 FR-004/FR-005).
    pub edges_dropped_unresolvable: usize,
    /// Edges whose two differently named endpoints resolved to the same entity UUID at Phase C
    /// commit time (dedup merges or salvage collapsing them onto one entity), dropped rather than
    /// inserted as a self-loop (issue #666). Distinct from the name-level self-reference filter
    /// (never counted) and from `edges_dropped_unresolvable`; carries no `dropped_edges` entry.
    pub edges_dropped_self_loop: usize,
    /// Per-edge detail behind `edges_dropped_unresolvable` above — one entry per edge counted
    /// there, in extraction order, carrying the edge's extracted content and which endpoint(s)
    /// failed to resolve (issue #411 FR-001/FR-002/FR-003/FR-006). Always present, empty when
    /// nothing was dropped (FR-005).
    pub dropped_edges: Vec<DroppedEdgeDetail>,
    /// Strict-mode edges whose relation type was outside the ontology's vocabulary even after
    /// alias normalisation, reclassified to `UNCLASSIFIED` rather than dropped (issue #310
    /// FR-004/FR-005 — distinct from `edges_dropped_unresolvable`'s issue #281 FR-004/FR-005
    /// above). The original relation type is preserved in the stored edge's `attributes` field,
    /// not lost.
    pub edges_reclassified_unclassified: usize,
    /// Strict-mode entities whose type was outside the ontology's declared vocabulary after
    /// normalisation, reclassified to `Unclassified` rather than dropped (issue #312 FR-004).
    /// The original entity type is preserved in the stored entity's `attributes` field, not
    /// lost.
    pub entities_reclassified_unclassified: usize,
    /// Entities dropped for failing required-field validation — either at parse time (the
    /// extractor's per-item salvage, which since #347 rejects a missing `name` *and* a blank or
    /// whitespace-only `name`) or by the empty-name `retain` below, in `add_episode`
    /// (defense-in-depth for `Extractor` implementors that bypass parse-time salvage, e.g.
    /// `ConfigurableExtractor`, `MockExtractor`). An item is only ever removed by one of the two
    /// layers, never both, so they feed this one counter disjointly —
    /// missing/`null`/empty-string/whitespace-only `name` all still produce a single observable
    /// outcome (#342 FR-003, FR-007; #347 FR-004).
    pub entities_dropped_malformed: usize,
    /// Edges dropped for failing required-field validation during extraction-response parsing —
    /// a missing `source_name`/`target_name`/`fact` (#342), or, since #347, a `source_name`,
    /// `target_name`, or `fact` that deserializes fine but is blank or whitespace-only (#347
    /// FR-001/FR-002). A blank endpoint name is counted here rather than in
    /// `edges_dropped_unresolvable` because it can never resolve in any graph, at any time — it
    /// is an invalid item, not an unresolved reference, and a parse-time-rejected edge never
    /// reaches the Phase C resolution code that populates `edges_dropped_unresolvable` anyway.
    /// An edge with multiple blank fields is counted once, not once per field.
    pub edges_dropped_malformed: usize,
    /// Phase B resolution outcomes by path for this chunk (issue #650).
    pub dedup_paths: DedupPathCounts,
}

struct ActiveWriteGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ActiveWriteGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

const DEDUP_THRESHOLD: f32 = 0.85;
/// Entity-side counterpart of `reprocess_relations::UNCLASSIFIED`, matching entity-type
/// (PascalCase) rather than relation-type (SCREAMING_SNAKE_CASE) casing conventions. Has
/// exactly one call site today; promote to a shared export if a future entity-side
/// canonicalize pass needs it (see ADR-0312).
const ENTITY_UNCLASSIFIED: &str = "Unclassified";

static HYBRID_THRESHOLD: OnceLock<usize> = OnceLock::new();

fn hybrid_threshold() -> usize {
    *HYBRID_THRESHOLD.get_or_init(|| {
        std::env::var("LIMINIS_DEDUP_HYBRID_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000)
    })
}

enum DedupDecision {
    Merge {
        existing_uuid: String,
        /// The summary this merge leaves the entity with (consolidated or fallback, always
        /// within `MERGED_SUMMARY_CAP`) — issue #651.
        merged_summary: String,
        /// `Some` only on the *last* merge into a uuid within a chunk whose final summary differs
        /// from the stored one: Phase C then writes `summary` and `summary_embedding` in a single
        /// `SET`. `None` means "nothing to write" (summary unchanged, or superseded by a later
        /// merge into the same uuid in this chunk).
        summary_embedding: Option<Vec<f32>>,
    },
    Insert {
        // Boxed: `EntityRow` grew past clippy::large_enum_variant's threshold once
        // `summary_embedding` (issue #470) was added, and `Merge`'s variant is much smaller.
        row: Box<EntityRow>,
    },
}

/// Produces the summary a merge leaves behind (issue #651): `Ok(None)` when the stored summary
/// stays as it is (empty or already-contained incoming), otherwise the new bounded summary.
///
/// Runs in Phase B — lock-free and cancellable, exactly like `dedup.is_duplicate`. The extractor
/// is consulted only when both sides carry text and the incoming one adds information. Any
/// failure — `Unconfigured` (silent), a transport/parse error, an empty reply — degrades to the
/// deterministic `fallback_merge`; a failed consolidation never fails the chunk.
async fn merged_summary_for(
    state: &AppState,
    entity_name: &str,
    current: &str,
    incoming: &str,
) -> Result<Option<String>, Error> {
    use crate::summary_merge::{
        decide_consolidation, fallback_merge, finalize_consolidation, ConsolidationPlan,
    };
    let merged = match decide_consolidation(current, incoming) {
        ConsolidationPlan::Keep => return Ok(None),
        ConsolidationPlan::UseIncoming(s) => s,
        ConsolidationPlan::Consolidate => {
            let reply = tokio::select! {
                r = state.extractor.consolidate_summary(entity_name, current, incoming) => r,
                _ = state.cancel_token.cancelled() => {
                    state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::Cancelled);
                }
            };
            match reply {
                Ok(text) => finalize_consolidation(&text)
                    .unwrap_or_else(|| fallback_merge(current, incoming)),
                Err(Error::Config(_)) => fallback_merge(current, incoming),
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: summary consolidation failed for '{entity_name}', \
                         using bounded fallback: {e}"
                    );
                    fallback_merge(current, incoming)
                }
            }
        }
    };
    Ok((merged != current).then_some(merged))
}

/// Resolves one merge decision in Phase B: advances the per-uuid running summary through
/// [`merged_summary_for`] and returns the `Merge` decision with no embedding yet (the post-loop
/// batch attaches it to the latest merge per uuid). Records `decision_idx` as that uuid's latest.
async fn merge_into(
    state: &AppState,
    merge_state: &mut std::collections::HashMap<String, (String, String, usize)>,
    decision_idx: usize,
    existing: &EntityRow,
    incoming: &str,
) -> Result<DedupDecision, Error> {
    let entry = merge_state.entry(existing.uuid.clone()).or_insert_with(|| {
        (
            existing.summary.clone(),
            existing.summary.clone(),
            decision_idx,
        )
    });
    if let Some(next) = merged_summary_for(state, &existing.name, &entry.1, incoming).await? {
        entry.1 = next;
    }
    // Pass 2 of Phase B (#652) merges judged candidates after the pass-1 ones, so decision
    // indices arrive out of order; the *highest* index is the last Phase C write for this uuid.
    entry.2 = entry.2.max(decision_idx);
    Ok(DedupDecision::Merge {
        existing_uuid: existing.uuid.clone(),
        merged_summary: entry.1.clone(),
        summary_embedding: None,
    })
}

/// Outcome of resolving one edge endpoint name in Phase C (issue #616). `Ambiguous` means the
/// name maps to entities of more than one kind; the edge is dropped rather than guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointResolution {
    Found(String),
    Missing,
    Ambiguous,
}

impl EndpointResolution {
    fn into_uuid(self) -> Option<String> {
        match self {
            EndpointResolution::Found(u) => Some(u),
            _ => None,
        }
    }
}

/// Looks an edge endpoint name up in the persisted graph (issue #666: shared by the pre-lock
/// exact-match probe that precedes salvage and by Phase C's authoritative re-resolution, so the
/// two cannot drift on eligibility). Same group, case-insensitive; with only the default kind it
/// is the pre-#616 scan-fallback lookup, otherwise the multi-kind lookup where more than one hit
/// is `Ambiguous` (issue #616).
fn resolve_stored_endpoint(
    conn: &crate::db::Conn<'_>,
    raw_name: &str,
    group_id: &str,
    endpoint_kinds: &[String],
) -> Result<EndpointResolution, Error> {
    if endpoint_kinds.len() == 1 {
        // No identity-bearing kinds in this group: exactly the pre-#616 lookup.
        return Ok(
            match conn.get_entity_by_name_ci_with_scan_fallback(
                raw_name,
                group_id,
                crate::types::DEFAULT_KIND,
            )? {
                Some(existing) => EndpointResolution::Found(existing.uuid),
                None => EndpointResolution::Missing,
            },
        );
    }
    // Default kind plus the group's identity-bearing kinds (issue #616). More than one hit is
    // ambiguous — never picked between.
    let mut hits = conn.resolve_entities_by_name_in_kinds(raw_name, group_id, endpoint_kinds)?;
    Ok(match hits.len() {
        0 => EndpointResolution::Missing,
        1 => EndpointResolution::Found(hits.remove(0).uuid),
        _ => EndpointResolution::Ambiguous,
    })
}

/// Result of Phase B's per-entity resolution attempt.
/// Name-matched entities skip the async dedup-adapter check entirely.
enum PhaseBResult {
    /// Exact case-insensitive name match found in the persisted graph.
    NameMatch { existing: EntityRow },
    /// No name match; embedding-based candidate (may be None if no similar entity exists).
    /// `vetoed` is the number of above-threshold candidates the identifier-mismatch veto
    /// discarded (issue #650).
    EmbeddingCandidate {
        candidate: Option<EntityRow>,
        vetoed: usize,
    },
}

/// Validates and returns a timestamp string from LLM output.
///
/// Returns `None` for empty strings or values that cannot be parsed as RFC 3339,
/// so invalid LLM output does not reach the DB's `timestamp()` call.
fn validate_llm_timestamp(s: Option<String>) -> Option<String> {
    let s = s?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if DateTime::parse_from_rfc3339(trimmed).is_ok() {
        Some(trimmed.to_string())
    } else {
        eprintln!(
            "liminis-context-graph: dropping invalid LLM timestamp: {:?}",
            trimmed
        );
        None
    }
}

/// Resolves each extracted entity name against the persisted graph: an exact case-insensitive
/// name match short-circuits to `NameMatch`, otherwise falls back to embedding-based candidate
/// lookup (hybrid HNSW+FTS above the dedup threshold, brute-force cosine scan below it).
///
/// Runs the whole batch inside a single `spawn_blocking` closure. On a missing-index error
/// (the hybrid path depends on `entity_name_embedding_idx`), the caller retries by calling this
/// function again in full — there is no partial/mid-batch resume.
async fn resolve_phase_b(
    db: Arc<Db>,
    group_id: String,
    entity_names: Vec<String>,
    entity_kinds: Vec<String>,
    name_embeddings: Vec<Vec<f32>>,
    use_hybrid: bool,
) -> Result<Vec<PhaseBResult>, Error> {
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let mut out = Vec::with_capacity(entity_names.len());
        for (i, name) in entity_names.iter().enumerate() {
            let trimmed = name.trim();
            // Name-first resolution: case-insensitive exact match short-circuits embedding lookup.
            // Resolution is scoped to the entity's own kind (issue #616): an identity-bearing
            // kind only ever matches its own kind, and the default kind only the default kind.
            let kind = entity_kinds[i].as_str();
            if let Some(existing) = conn.get_entity_by_name_ci(trimmed, &group_id, kind)? {
                out.push(PhaseBResult::NameMatch { existing });
                continue;
            }
            // Embedding-based resolution fallback.
            let emb = &name_embeddings[i];
            // Name-aware selection: candidates failing the identifier-mismatch veto are discarded
            // before the best is picked (issue #650, ADR-0650).
            let selection = if use_hybrid {
                conn.hybrid_dedup_similar_entity_kind_for_name(
                    emb,
                    trimmed,
                    &group_id,
                    DEDUP_THRESHOLD,
                    kind,
                )?
            } else {
                conn.brute_force_similar_entity_kind_for_name(
                    emb,
                    trimmed,
                    &group_id,
                    DEDUP_THRESHOLD,
                    kind,
                )?
            };
            out.push(PhaseBResult::EmbeddingCandidate {
                candidate: selection.candidate,
                vetoed: selection.vetoed,
            });
        }
        Ok::<_, Error>(out)
    })
    .await?
}

/// Runs the full add_episode pipeline in three async phases (AD-4).
///
/// Phase A: concurrent HTTP (no lock) — embed body, extract entities/edges, embed names/facts.
/// Phase B: async dedup (no lock) — fetch cosine candidates, call DedupAdapter per candidate.
/// Phase C: commit (exclusive write lock) — apply dedup decisions, insert edges, episodic, MENTIONS.
///
/// Returns the episode UUID.
#[allow(clippy::too_many_arguments)]
pub async fn add_episode(
    state: Arc<AppState>,
    name: &str,
    body: &str,
    source: &str,
    source_description: &str,
    reference_time: &str,
    group_id: &str,
    source_type: SourceType,
    custom_instructions: Option<&str>,
    attributes: &str,
) -> Result<AddEpisodeResult, Error> {
    // Track write in flight so rebuild_from_wal can gate on active writes.
    state.active_writes.fetch_add(1, Ordering::Relaxed);
    let _active_guard = ActiveWriteGuard(Arc::clone(&state.active_writes));

    // Identity-bearing-set guard (issue #616, D1): a group whose ontology changed the
    // identity-bearing set in a way that would reinterpret existing entities is refused here,
    // before any extraction, without touching data or other groups.
    state.check_identity(group_id)?;

    // ── Phase A: concurrent HTTP (no lock) ────────────────────────────────────
    // Resolves this group's own ontology file if one exists, else falls back to the
    // workspace-wide ontology (FR-001, FR-002, FR-005) — governs extraction guidance, strict-mode
    // validation, and (via `ancestor_map` below) canonicalization for this group only.
    let resolved_ontology = state.resolve_ontology(group_id);
    let ontology_ref = resolved_ontology.as_deref();
    let extract_opts = ExtractOptions {
        episode_body: body,
        group_id,
        source_type,
        custom_instructions,
        reference_time,
        ontology: ontology_ref,
        chunk_key: Some(name),
    };
    let (content_embedding, extraction_outcome): (Vec<f32>, ExtractionOutcome) = tokio::select! {
        result = async {
            tokio::try_join!(
                state.embedder.embed(body),
                state.extractor.extract(extract_opts)
            )
        } => result?,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };
    let mut extraction = extraction_outcome.result;
    // Parse-time salvage count (#342 FR-003) — folded together with the empty-name `retain`'s
    // count below (FR-007) into one counter, since both represent the same observable outcome
    // (a malformed item never reaching storage) even though they're caught at different layers.
    let mut entities_dropped_malformed = extraction_outcome.entities_dropped_malformed;
    let edges_dropped_malformed = extraction_outcome.edges_dropped_malformed;

    // `original_relation_type` is deserialized directly from raw, untrusted extractor/LLM JSON
    // (`#[serde(default)]`, no `deny_unknown_fields`) — a hallucinated or prompt-injected key of
    // that name in the model's edge output must never survive to storage. Clear it here,
    // unconditionally and regardless of ontology mode/config, before any mode-specific filtering
    // runs below. It is set again, only by this function's own logic, in the strict-mode
    // out-of-vocabulary branch further down.
    for e in extraction.edges.iter_mut() {
        e.original_relation_type = None;
    }

    // `original_entity_type` is deserialized directly from raw, untrusted extractor/LLM JSON
    // (`#[serde(default)]`, no `deny_unknown_fields`) — a hallucinated or prompt-injected key of
    // that name in the model's entity output must never survive to storage. Clear it here,
    // unconditionally and regardless of ontology mode/config, before any mode-specific filtering
    // runs below. It is set again, only by this function's own logic, in the strict-mode
    // out-of-vocabulary branch further down.
    for e in extraction.entities.iter_mut() {
        e.original_entity_type = None;
    }

    // Drop entities with empty or whitespace-only names before any strict-mode filtering that
    // tallies counts (also before edge validation, so any edges referencing them are dropped as
    // unresolvable — spec edge case: treat empty-name extraction as a failure and do not create
    // a node for it). Doing this before the reclassify loop below, rather than after, matters:
    // an empty-named entity must never be counted toward `entities_reclassified_unclassified`
    // since it never reaches storage — counting it first and dropping it after would desync the
    // tally from what's actually persisted (review finding on issue #312).
    //
    // This drop is disjoint from the parse-time salvage folded into `entities_dropped_malformed`
    // above: parse-time salvage removes items that failed to deserialize (missing/`null` name)
    // and, since #347, items that deserialized fine but carry a blank/whitespace-only name too —
    // for the two real providers (Anthropic, OAI-compatible), this `retain` is now a no-op in
    // practice. It stays load-bearing as defense-in-depth for `Extractor` implementors that
    // bypass parse-time salvage entirely (`ConfigurableExtractor`, `MockExtractor`, other test
    // doubles that build `ExtractionResult` directly), which never call `salvage_items`. An item
    // is only ever removed by one of the two layers, never both, so folding this count into the
    // same counter (#342 FR-007, #347 FR-004) makes missing/`null`/empty-string/whitespace-only
    // `name` all produce one observable outcome without double-counting.
    let before_empty_name_retain = extraction.entities.len();
    extraction.entities.retain(|e| !e.name.trim().is_empty());
    entities_dropped_malformed += before_empty_name_retain - extraction.entities.len();

    // Strict-mode entity filtering (issue #312): an entity is never dropped for its entity_type
    // alone. An entity whose normalized type is empty or the literal "Entity" means "no specific
    // type" — as it does everywhere else in this function — and passes through unchanged (FR-007),
    // with `entity_type` rewritten to its normalized form (empty or "Entity") so a raw case/
    // separator variant (e.g. "entity", "ENTITY") can never leak into `EntityRow.labels` via
    // `make_insert_row`'s raw-string check (review finding on issue #312). A non-empty,
    // non-matching type is reclassified to `Unclassified`, with the original label preserved on
    // `original_entity_type` for later storage in `attributes` (FR-002/FR-003) — never deleted,
    // consistent with ADR-0033/ADR-0037/ADR-0310/ADR-0312. Unlike the edge-side reclassify tally
    // (deferred to Phase C per ADR-0051, since an edge can still be dropped afterward), the
    // entity tally is counted directly here: because the empty-name retain above already ran,
    // every entity reaching this loop is guaranteed to be persisted, so there's no desync risk.
    let mut entities_reclassified_unclassified = 0usize;
    //
    // `extract: false` types (#637, ADR-0637) are excluded from the strict-mode vocabulary, so a
    // stray label naming one is reclassified here like any other off-vocabulary type. The same
    // reclassification also applies in open mode (below), where the raw label would otherwise be
    // stamped on the entity verbatim — the only open-mode labels touched are `extract: false` ones.
    if let Some(onto) = ontology_ref {
        // Strict mode with no extractable entity type has no vocabulary to filter against (the
        // prompt falls back to the default one), so it takes the same direct-label path as open mode.
        if onto.mode != OntologyMode::Strict || onto.extractable_entity_types().next().is_none() {
            for e in extraction.entities.iter_mut() {
                if onto.is_extract_false_entity(&e.entity_type) {
                    eprintln!(
                        "liminis-context-graph: ontology: reclassifying entity '{}' to Unclassified (type '{}' is extract: false)",
                        e.name, e.entity_type
                    );
                    e.original_entity_type = Some(e.entity_type.clone());
                    e.entity_type = ENTITY_UNCLASSIFIED.to_string();
                    entities_reclassified_unclassified += 1;
                }
            }
        }
        if onto.mode == OntologyMode::Strict && onto.extractable_entity_types().next().is_some() {
            let vocab = onto.extractable_entity_type_names();
            for e in extraction.entities.iter_mut() {
                let normalized = normalize_entity_type(&e.entity_type);
                if normalized.is_empty() || normalized == "Entity" {
                    // No specific type extracted — resolves as a plain untyped Entity, same as
                    // every other ontology mode. Not a reclassification.
                    e.entity_type = normalized;
                    continue;
                }
                if vocab.contains(&normalized) {
                    e.entity_type = normalized;
                } else {
                    eprintln!(
                        "liminis-context-graph: ontology strict: reclassifying entity '{}' to Unclassified (type '{}' not in vocabulary)",
                        e.name, e.entity_type
                    );
                    e.original_entity_type = Some(e.entity_type.clone());
                    e.entity_type = ENTITY_UNCLASSIFIED.to_string();
                    entities_reclassified_unclassified += 1;
                }
            }
        }
    }

    // Strict-mode relation_type filtering (issue #310): an edge is never dropped for its
    // relation_type alone. First alias-normalise against the ontology's declared alias map
    // (FR-001, reusing `canonicalize::build_alias_map` rather than a second parallel map) so a
    // declared alias like `LAUNCHED_BY` is rewritten to its canonical `LAUNCHED` instead of
    // being destroyed. An edge whose relation_type is outside the vocabulary even after
    // normalisation is reclassified to `UNCLASSIFIED`, with the original label preserved on
    // `original_relation_type` for later storage in `attributes` (FR-004) — never deleted,
    // consistent with ADR-0033/ADR-0037/ADR-0310.
    //
    // This pass only rewrites `relation_type`/`original_relation_type`; it deliberately does not
    // tally `edges_reclassified_unclassified` here. An edge marked here can still be dropped
    // afterward as self-referential or (in Phase C) for an unresolvable endpoint, and counting
    // here would desync the tally from what's actually persisted — the same failure mode
    // ADR-0051 fixed for `edges_dropped_unresolvable` by making Phase C the sole authoritative
    // counting point. The tally is instead taken in Phase C, alongside `edges_inserted`.
    //
    // `extract: false` relation types (#637, ADR-0637) are absent from `build_alias_map`, so in
    // strict mode a stray label naming one falls through to `UNCLASSIFIED` here. In open mode only
    // those labels are reclassified (the tally in Phase C keys on `original_relation_type`).
    if let Some(onto) = ontology_ref {
        if onto.mode != OntologyMode::Strict || onto.extractable_relation_types().next().is_none() {
            for e in extraction.edges.iter_mut() {
                let Some(original) = e.relation_type.clone() else {
                    continue;
                };
                if onto.is_extract_false_relation(&original) {
                    eprintln!(
                        "liminis-context-graph: ontology: reclassifying edge '{}' → '{}' to UNCLASSIFIED (relation_type '{}' is extract: false)",
                        e.source_name, e.target_name, original
                    );
                    e.relation_type = Some(UNCLASSIFIED.to_string());
                    e.original_relation_type = Some(original);
                }
            }
        }
        if onto.mode == OntologyMode::Strict && onto.extractable_relation_types().next().is_some() {
            let alias_map = build_alias_map(onto);
            for e in extraction.edges.iter_mut() {
                let original = e.relation_type.clone();
                let normalized = original
                    .as_deref()
                    .map(normalize_relation_type)
                    .unwrap_or_default();
                match alias_map.get(&normalized) {
                    Some(canonical) => {
                        e.relation_type = Some(canonical.clone());
                    }
                    None => {
                        eprintln!(
                            "liminis-context-graph: ontology strict: reclassifying edge '{}' → '{}' to UNCLASSIFIED (relation_type '{}' not in vocabulary)",
                            e.source_name, e.target_name, original.as_deref().unwrap_or("")
                        );
                        e.relation_type = Some(UNCLASSIFIED.to_string());
                        e.original_relation_type = if normalized.is_empty() {
                            None
                        } else {
                            original
                        };
                    }
                }
            }
        }
    }

    // Load the DB handle here (rather than at Phase B, below) — Phase B's entity-count check
    // and dedup resolution reuse this same Arc. Phase C, below, reloads its own handle
    // (`db_c`) right before acquiring the write lock, deliberately, in case a concurrent
    // `clear_all` swapped `state.db` in the meantime.
    let db_shared = state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })?;

    // name_embeddings is computed here — before edge validation, rather than after it as
    // before — so the salvage step below can cosine-match an off-list edge endpoint against
    // the batch's own entity name embeddings without re-embedding anything (order-only change;
    // extraction.entities is already final by this point).
    //
    // The summary_embedding pass (issue #470) batches alongside it via a second `embed_batch`
    // call, joined concurrently with `futures::future::try_join` rather than run sequentially —
    // the two batches are independent, so running them concurrently keeps this chunk's added
    // latency to one batch round-trip instead of two (issue #445). Per ADR-0314, an extracted
    // `summary` legitimately defaults to `""` — that's never sent to the embedder (would waste a
    // batch slot encoding nothing); those entities are excluded from the summary batch and get a
    // same-dimension zero vector instead, the same sentinel `insert_entity` falls back to for any
    // unset `summary_embedding`. `summary_indices` records each included entity's original
    // position so the batch's output (dense, in submission order) can be scattered back to the
    // right index — the one non-mechanical step in this conversion (see #445 research/plan).
    let entity_names: Vec<String> = extraction.entities.iter().map(|e| e.name.clone()).collect();
    // Per-entity kind (issue #616, FR-002/FR-003): the entity's *primary* extracted type, if and
    // only if the group's resolved ontology marks that type identity-bearing; otherwise the
    // default kind. Decided here, once, from the primary type alone — parent-hierarchy ancestors
    // never confer kind and no label or declaration order is consulted. `Unclassified` (the
    // strict-mode catch-all) is never identity-bearing.
    let entity_kinds: Vec<String> = extraction
        .entities
        .iter()
        .map(|e| {
            if e.entity_type == ENTITY_UNCLASSIFIED {
                return crate::types::DEFAULT_KIND.to_string();
            }
            ontology_ref
                .and_then(|o| o.identity_kind(&e.entity_type))
                .unwrap_or_else(|| crate::types::DEFAULT_KIND.to_string())
        })
        .collect();
    // Kinds an edge endpoint that is not in this batch may resolve against (cross-batch
    // resolution): the default kind plus this group's identity-bearing kinds. Never asserted
    // kinds extraction did not create (#615: extraction stays out of them).
    let endpoint_kinds: Vec<String> = {
        let mut k = vec![crate::types::DEFAULT_KIND.to_string()];
        if let Some(o) = ontology_ref {
            k.extend(o.identity_set());
        }
        k
    };
    let name_refs: Vec<&str> = entity_names.iter().map(|s| s.as_str()).collect();
    let summary_indices: Vec<usize> = extraction
        .entities
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.summary.trim().is_empty())
        .map(|(i, _)| i)
        .collect();
    let summary_refs: Vec<&str> = summary_indices
        .iter()
        .map(|&i| extraction.entities[i].summary.as_str())
        .collect();

    let (name_embeddings, summary_batch_embeddings) = tokio::select! {
        r = futures::future::try_join(
            state.embedder.embed_batch(&name_refs),
            state.embedder.embed_batch(&summary_refs),
        ) => r?,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };

    let mut summary_embeddings: Vec<Vec<f32>> =
        vec![vec![0.0f32; state.embedder.dim()]; extraction.entities.len()];
    for (idx, emb) in summary_indices.into_iter().zip(summary_batch_embeddings) {
        summary_embeddings[idx] = emb;
    }

    // Post-extraction edge validation (pre-lock, advisory only): drop self-referential edges —
    // a pure, DB-independent check that's always correct — then *salvage*, rather than
    // permanently drop, edges whose endpoint name is absent from this batch's own entity list.
    // An off-list endpoint's name embedding is cosine-matched against the batch's entity
    // name_embeddings, reusing DEDUP_THRESHOLD (the same threshold already used for entity
    // dedup); a match rewrites the edge's endpoint to that entity's canonical name in place.
    // An off-list endpoint that exactly matches the stored graph is not salvaged at all (#666).
    // Anything that doesn't salvage-match is left untouched and passed through to Phase C
    // (write-lock held), which is now the *sole* point that resolves an endpoint — falling back
    // to the persisted graph — or finally drops the edge, making `edges_dropped_unresolvable`
    // authoritative (FR-003, FR-005) instead of one of two independent, easily-desynced passes.
    extraction.edges.retain(|edge| {
        if normalize_name(&edge.source_name) == normalize_name(&edge.target_name) {
            eprintln!(
                "liminis-context-graph: dropping self-referential edge: '{}' → '{}'",
                edge.source_name, edge.target_name
            );
            return false;
        }
        true
    });

    let mut dedup_paths = DedupPathCounts::default();

    if !extraction.edges.is_empty() {
        // Keyed by the same normalization applied to a name before it ever reaches the model
        // (control-char strip + trim + lowercase, `prompts::normalize_name`) — an entity name
        // containing a control character is shown to the model with that character stripped, so
        // matching against the *original* name here would spuriously miss it.
        let entity_name_set: std::collections::HashSet<String> = extraction
            .entities
            .iter()
            .map(|e| normalize_name(&e.name))
            .collect();

        // Collect the unique off-batch endpoint names needing salvage, keyed by the normalized
        // name, so a batch with many edges naming the same missing endpoint costs one
        // embed+match, not one per edge.
        let mut missing_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // Every distinct raw spelling behind a normalized key (issue #666): the normalized key
        // strips control characters, but the stored-graph probe below uses the DB's
        // `trim().to_lowercase()` identity, so `A\u{1}pple` and `Apple` share a key yet only
        // one may be an exact stored hit. Probing all spellings keeps the exact-match-beats-
        // salvage guarantee independent of which spelling the map happened to keep.
        let mut missing_variants: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for edge in &extraction.edges {
            for name in [&edge.source_name, &edge.target_name] {
                let key = normalize_name(name);
                if !entity_name_set.contains(&key) {
                    let raw = name.trim().to_string();
                    let variants = missing_variants.entry(key.clone()).or_default();
                    if !variants.contains(&raw) {
                        variants.push(raw.clone());
                    }
                    missing_names.entry(key).or_insert(raw);
                }
            }
        }

        // Collected into a Vec with one fixed ordering (rather than iterated directly off the
        // HashMap) so the batch call's output — dense, in submission order — can be zipped back
        // to the correct key by position; HashMap iteration order is unspecified per-run, so
        // iterating the map twice (once to build the request, once to zip the response) could
        // silently misassign an embedding to the wrong endpoint name (#445 research/plan).
        let missing_names: Vec<(String, String)> = missing_names.into_iter().collect();

        // Exact stored-graph match beats cosine salvage (issue #666). An off-list endpoint that
        // already exists in the persisted graph — same group, eligible kinds, case-insensitive,
        // the same lookup Phase C uses — is not a salvage candidate: rewriting it onto a merely
        // similar batch entity would be a silent wrong re-point. An `Ambiguous` hit (same name
        // under more than one kind) counts too: Phase C drops it, and salvaging it onto a batch
        // entity would be the guess ADR-0615 forbids. The name is left untouched; Phase C
        // re-resolves it under the write lock and stays the sole authority (ADR-0051). Lock-free
        // like Phase B, so the same ADR-0029 TOCTOU caveat applies. Exact hits need no embedding.
        let missing_names: Vec<(String, String)> = if missing_names.is_empty() {
            missing_names
        } else {
            let probe_db = Arc::clone(&db_shared);
            let probe_gid = group_id.to_string();
            let probe_kinds = endpoint_kinds.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>, Error> {
                let conn = probe_db.connect()?;
                let mut remaining = Vec::with_capacity(missing_names.len());
                for (lower, original) in missing_names {
                    // A key is an exact hit if any raw spelling behind it is: salvage rewrites
                    // by key, so a hit on one spelling must not be salvaged over via another.
                    let mut exact = None;
                    for variant in missing_variants.get(&lower).into_iter().flatten() {
                        match resolve_stored_endpoint(&conn, variant, &probe_gid, &probe_kinds)? {
                            EndpointResolution::Missing => {}
                            EndpointResolution::Found(_) | EndpointResolution::Ambiguous => {
                                exact = Some(variant.clone());
                                break;
                            }
                        }
                    }
                    match exact {
                        None => remaining.push((lower, original)),
                        Some(hit) => eprintln!(
                            "liminis-context-graph: off-list edge endpoint '{hit}' matches the stored graph exactly — skipping salvage"
                        ),
                    }
                }
                Ok(remaining)
            })
            .await??
        };

        let missing_refs: Vec<&str> = missing_names.iter().map(|(_, o)| o.as_str()).collect();
        let missing_embeddings = if missing_refs.is_empty() {
            Vec::new()
        } else {
            tokio::select! {
                r = state.embedder.embed_batch(&missing_refs) => r?,
                _ = state.cancel_token.cancelled() => {
                    state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::Cancelled);
                }
            }
        };

        let mut salvage_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for ((lower, original), emb) in missing_names.into_iter().zip(missing_embeddings) {
            let mut best: Option<(f32, &str)> = None;
            let mut any_vetoed = false;
            for (i, candidate_emb) in name_embeddings.iter().enumerate() {
                let score = crate::db::cosine_similarity(&emb, candidate_emb);
                let is_better = match best {
                    Some((b, _)) => score > b,
                    None => true,
                };
                if score >= DEDUP_THRESHOLD {
                    // Identifier-mismatch veto (issue #650): `ADR 2018` must not be rewritten
                    // onto a batch entity `ADR 2019`.
                    if crate::identifier_veto::identifier_mismatch(
                        &original,
                        &extraction.entities[i].name,
                    ) {
                        any_vetoed = true;
                    } else if is_better {
                        best = Some((score, extraction.entities[i].name.as_str()));
                    }
                }
            }
            if best.is_none() && any_vetoed {
                dedup_paths.salvage_vetoed += 1;
            }
            if let Some((score, canonical)) = best {
                eprintln!(
                    "liminis-context-graph: salvaging off-list edge endpoint '{}' → '{}' (cosine similarity {:.3})",
                    original, canonical, score
                );
                salvage_map.insert(lower, canonical.to_string());
            }
        }

        if !salvage_map.is_empty() {
            for edge in extraction.edges.iter_mut() {
                if let Some(canonical) = salvage_map.get(&normalize_name(&edge.source_name)) {
                    edge.source_name = canonical.clone();
                }
                if let Some(canonical) = salvage_map.get(&normalize_name(&edge.target_name)) {
                    edge.target_name = canonical.clone();
                }
            }

            // Rewriting an off-list endpoint to its canonical entity name can make two
            // previously-distinct endpoints collide (e.g. "Global Warming" and "Climate Change"
            // both salvage to the same batch entity) — re-run the self-referential filter after
            // salvage so a rewritten edge like that doesn't slip past the earlier check and get
            // inserted as a self-loop in Phase C. Phase C's UUID-level guard (#666) would still
            // catch it, but only as a counted `edges_dropped_self_loop`; this name-level filter
            // drops such collisions silently, as it always has.
            extraction.edges.retain(|edge| {
                if normalize_name(&edge.source_name) == normalize_name(&edge.target_name) {
                    eprintln!(
                        "liminis-context-graph: dropping self-referential edge after salvage: '{}' → '{}'",
                        edge.source_name, edge.target_name
                    );
                    return false;
                }
                true
            });
        }
    }

    let edge_facts: Vec<String> = extraction.edges.iter().map(|e| e.fact.clone()).collect();
    let edge_fact_refs: Vec<&str> = edge_facts.iter().map(|s| s.as_str()).collect();
    let fact_embeddings = tokio::select! {
        r = state.embedder.embed_batch(&edge_fact_refs) => r?,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };

    // ── Phase B: async dedup (no lock) ────────────────────────────────────────
    if state.cancel_token.is_cancelled() {
        state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
        return Err(Error::Cancelled);
    }
    // Fetch cosine candidates in a blocking pass, then verify each with DedupAdapter.
    // `db_shared` was already loaded above, before edge validation (#209).
    let gid_b = group_id.to_string();
    let db_b = Arc::clone(&db_shared);
    let entity_count = tokio::task::spawn_blocking(move || {
        let conn = db_b.connect()?;
        conn.entity_count_in_group(&gid_b)
    })
    .await??;

    let use_hybrid = entity_count >= hybrid_threshold();
    // Above the hybrid-dedup threshold, this query depends on entity_name_embedding_idx (and the
    // FTS indexes hybrid_dedup_similar_entity also uses). On a workspace where nothing has ever
    // built those indices, retry once via the same missing-index auto-heal the search handlers
    // use (ADR-0025) rather than failing the chunk (#208).
    let phase_b_attempt = resolve_phase_b(
        Arc::clone(&db_shared),
        group_id.to_string(),
        entity_names.clone(),
        entity_kinds.clone(),
        name_embeddings.clone(),
        use_hybrid,
    )
    .await;
    let phase_b_results: Vec<PhaseBResult> = match phase_b_attempt {
        Ok(results) => results,
        Err(e) if is_missing_index_error(&e) => {
            if state.indices_built.load(Ordering::Acquire) {
                // Indices are supposedly built but the query still failed this way — a redundant
                // rebuild wouldn't help (mirrors the search handlers' identical quirk).
                return Err(Error::Ipc(MISSING_INDEX_USER_MSG.to_string()));
            }
            build_indices_once(&state).await?;
            // Reload the db in case a concurrent clear_all swapped it while we were building.
            let db_retry = load_db(&state)?;
            resolve_phase_b(
                db_retry,
                group_id.to_string(),
                entity_names.clone(),
                entity_kinds.clone(),
                name_embeddings.clone(),
                use_hybrid,
            )
            .await
            .map_err(|e2| {
                if is_missing_index_error(&e2) {
                    Error::Ipc(MISSING_INDEX_USER_MSG.to_string())
                } else {
                    e2
                }
            })?
        }
        Err(e) => return Err(e),
    };

    // Async dedup verification (no lock), in two passes (#652). Pass 1 resolves everything that
    // needs no judgement and collects the embedding-path candidates that do; pass 2 judges them
    // in one batched extractor call. The LLM call therefore stays in Phase B, outside the Phase C
    // write lock.
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();
    // Per-existing-uuid running summary (issue #651): a second merge into the same entity within
    // this chunk consolidates on top of the first result, not on the stale pre-chunk summary
    // (which would let the last Phase C `SET` silently discard the earlier merge).
    // `merge_state` maps uuid → (summary before this chunk, running summary, index of the latest
    // Merge decision for that uuid).
    let mut merge_state: std::collections::HashMap<String, (String, String, usize)> =
        std::collections::HashMap::new();
    let make_insert_row = |i: usize| {
        let extracted = &extraction.entities[i];
        let name_embedding = name_embeddings[i].clone();
        let summary_embedding = summary_embeddings[i].clone();
        DedupDecision::Insert {
            row: Box::new(EntityRow {
                ingested_at: String::new(),
                uuid: uuid::Uuid::new_v4().to_string(),
                name: extracted.name.clone(),
                group_id: gid_owned.clone(),
                // Ontology-driven kind (issue #616): the primary type when identity-bearing,
                // otherwise the default kind (issue #615, FR-010: extraction stays out of
                // asserted kinds).
                kind: entity_kinds[i].clone(),
                labels: {
                    let mut labels = vec!["Entity".to_string()];
                    // An identity-bearing kind is spelled by its normalized type name, so
                    // `kind ∈ labels` holds in open mode too, where the raw string is kept.
                    let label_type = if entity_kinds[i] != crate::types::DEFAULT_KIND {
                        entity_kinds[i].as_str()
                    } else {
                        extracted.entity_type.as_str()
                    };
                    if !label_type.is_empty() && label_type != "Entity" {
                        if let Some(ancestors) =
                            ontology_ref.and_then(|o| o.ancestor_map.get(label_type))
                        {
                            labels.extend(ancestors.iter().cloned());
                        }
                        labels.push(label_type.to_string());
                    }
                    labels
                },
                created_at: ref_time_owned.clone(),
                name_embedding,
                summary: extracted.summary.clone(),
                attributes: match &extracted.original_entity_type {
                    Some(orig) => serde_json::json!({ "original_entity_type": orig }).to_string(),
                    None => "{}".to_string(),
                },
                episode_uuids: vec![],
                source_descriptions: vec![],
                summary_embedding,
            }),
        }
    };
    let mut decisions: Vec<Option<DedupDecision>> = Vec::with_capacity(extraction.entities.len());
    // (entity index, pair) for each candidate that survived the veto and still needs a verdict.
    let mut pending: Vec<(usize, DuplicatePair)> = Vec::new();
    for (i, extracted) in extraction.entities.iter().enumerate() {
        if state.cancel_token.is_cancelled() {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
        let decision = match &phase_b_results[i] {
            PhaseBResult::NameMatch { existing } => {
                // Exact name match — resolve immediately, no dedup-adapter check needed.
                if !extracted.entity_type.is_empty()
                    && extracted.entity_type != "Entity"
                    && !existing.labels.contains(&extracted.entity_type)
                {
                    eprintln!(
                        "liminis-context-graph: entity resolution: type conflict for '{}': \
                         existing labels {:?}, extracted type '{}'",
                        existing.name, existing.labels, extracted.entity_type
                    );
                }
                dedup_paths.exact_name += 1;
                merge_into(
                    &state,
                    &mut merge_state,
                    decisions.len(),
                    existing,
                    &extracted.summary,
                )
                .await?
            }
            PhaseBResult::EmbeddingCandidate {
                candidate: Some(existing),
                ..
            } => {
                if normalize_name(&existing.name) == normalize_name(&extracted.name) {
                    // Identical normalized names: the same entity, no judgement needed (FR-005).
                    dedup_paths.embedding_merge += 1;
                    merge_into(
                        &state,
                        &mut merge_state,
                        decisions.len(),
                        existing,
                        &extracted.summary,
                    )
                    .await?
                } else {
                    pending.push((i, pair_from(existing, extracted)));
                    decisions.push(None);
                    continue;
                }
            }
            PhaseBResult::EmbeddingCandidate {
                candidate: None,
                vetoed,
            } => {
                if *vetoed > 0 {
                    dedup_paths.vetoed += 1;
                }
                make_insert_row(i)
            }
        };
        decisions.push(Some(decision));
    }

    if !pending.is_empty() {
        let pairs: Vec<DuplicatePair> = pending.iter().map(|(_, p)| p.clone()).collect();
        let verdicts = tokio::select! {
            v = state.dedup.judge_batch(&pairs) => v,
            _ = state.cancel_token.cancelled() => {
                state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Cancelled);
            }
        };
        let llm_mode = state.dedup.mode() == DedupMode::LlmVerified;
        for ((i, _), verdict) in pending.iter().zip(
            verdicts
                .into_iter()
                .chain(std::iter::repeat(DedupVerdict::Unknown)),
        ) {
            let PhaseBResult::EmbeddingCandidate {
                candidate: Some(existing),
                ..
            } = &phase_b_results[*i]
            else {
                unreachable!("pending entries are embedding candidates");
            };
            let extracted = &extraction.entities[*i];
            decisions[*i] = Some(match verdict {
                DedupVerdict::Duplicate => {
                    if llm_mode {
                        dedup_paths.llm_confirmed += 1;
                    } else {
                        dedup_paths.embedding_merge += 1;
                    }
                    merge_into(&state, &mut merge_state, *i, existing, &extracted.summary).await?
                }
                DedupVerdict::Distinct => {
                    if llm_mode {
                        dedup_paths.llm_rejected += 1;
                    } else {
                        dedup_paths.adapter_rejected += 1;
                    }
                    make_insert_row(*i)
                }
                DedupVerdict::Unknown => {
                    // A legacy per-pair adapter's `Err` lands here too; it is likewise an insert,
                    // but must not be reported as LLM activity while the mode is veto-only.
                    if llm_mode {
                        dedup_paths.llm_unavailable += 1;
                    } else {
                        dedup_paths.adapter_rejected += 1;
                    }
                    make_insert_row(*i)
                }
            });
        }
    }
    let mut decisions: Vec<DedupDecision> = decisions
        .into_iter()
        .map(|d| d.expect("every entity resolved by pass 2"))
        .collect();

    // Re-embed every merged summary that actually changed — one lock-free, cancellable batch
    // (issue #651, FR-005), outside `write_lock` per ADR-0543. Only the latest merge into a uuid
    // carries the embedding; an embedder error fails the chunk (as the pre-lock embedding pass
    // does) rather than leaving a silently stale vector.
    let mut changed: Vec<(usize, &str)> = merge_state
        .values()
        .filter(|(before, running, _)| before != running)
        .map(|(_, running, idx)| (*idx, running.as_str()))
        .collect();
    changed.sort_by_key(|(idx, _)| *idx);
    if !changed.is_empty() {
        let refs: Vec<&str> = changed.iter().map(|(_, s)| *s).collect();
        let embeddings = tokio::select! {
            r = state.embedder.embed_batch(&refs) => r?,
            _ = state.cancel_token.cancelled() => {
                state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Cancelled);
            }
        };
        let targets: Vec<(usize, String)> =
            changed.iter().map(|(i, s)| (*i, s.to_string())).collect();
        for ((idx, summary), emb) in targets.into_iter().zip(embeddings) {
            if let DedupDecision::Merge {
                merged_summary,
                summary_embedding,
                ..
            } = &mut decisions[idx]
            {
                *merged_summary = summary;
                *summary_embedding = Some(emb);
            }
        }
    }

    // Capture counts before extraction moves into the Phase C closure. `edges_extracted` is
    // no longer precomputed here — it's now the actual insert count Phase C returns, since
    // Phase C is the sole point that finally resolves or drops an edge (FR-004, FR-005).
    let nodes_extracted = extraction.entities.len();
    // Gates the `edges_reclassified_unclassified` tally below: without this, an edge whose
    // relation_type happens to already be the literal string "UNCLASSIFIED" (e.g. under `open`
    // mode, where the strict-mode reclassify filter never runs) would be miscounted as a
    // strict-mode reclassification. Captured now since `ontology_ref` isn't 'static and can't
    // move into the spawn_blocking closure.
    // The strict relation filter only runs when some relation type is extractable (#637).
    let is_strict_mode = ontology_ref.is_some_and(|o| {
        o.mode == OntologyMode::Strict && o.extractable_relation_types().next().is_some()
    });

    // ── Phase C: commit under write lock ─────────────────────────────────────
    let episode_uuid = uuid::Uuid::new_v4().to_string();
    let ep_uuid = episode_uuid.clone();
    let name_owned = name.to_string();
    let body_owned = body.to_string();
    let source_owned = source.to_string();
    let source_desc_owned = source_description.to_string();
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();
    // `attributes` is expected to already be a serialized JSON object (handlers.rs's
    // `attributes_param_to_string` guarantees this for the two IPC entry points), but
    // `add_episode` is a public library function — an internal/library caller passing `""` or
    // other non-JSON-object text would otherwise violate `EpisodicRow.attributes`'s documented
    // invariant of always being a parseable JSON object string. Normalize here so the invariant
    // holds regardless of caller (Copilot review, issue #528).
    let attributes_owned = match serde_json::from_str::<serde_json::Value>(attributes.trim()) {
        Ok(serde_json::Value::Object(_)) => attributes.to_string(),
        _ => "{}".to_string(),
    };
    let db_c = state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })?;

    let state_c = Arc::clone(&state);
    let gid_wal = gid_owned.clone();
    // Guard stays in async scope; spawn_blocking completes while it is held.
    // tokio::sync::RwLockWriteGuard is not 'static so it cannot move into the closure.
    // Cancellation is checked here — once the write guard is acquired the commit runs to
    // completion (FR-003: Phase C must not be torn mid-write).
    let _write_guard = tokio::select! {
        g = state.write_lock.write() => g,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };
    // Re-run the D1 guard now that the write lock is held (issue #616). The check at the top ran
    // before extraction, lock-free; a `knowledge_delete_by_group` / `knowledge_clear_all` that
    // completed in between removed this group's stamp and dropped its cached ontology. Without
    // this, Phase C would write identity-kind entities with no stamp, and the next resolve would
    // falsely refuse the group though its ontology never changed. After such a purge the group
    // has no carriers, so this re-stamps; otherwise it is a cache hit.
    state.check_identity(group_id)?;
    let (
        edges_inserted,
        edges_dropped_unresolvable,
        edges_reclassified_unclassified,
        dropped_edges,
        edges_dropped_self_loop,
    ) = tokio::task::spawn_blocking(
            move || -> Result<(usize, usize, usize, Vec<DroppedEdgeDetail>, usize), Error> {
        let conn = db_c.connect()?;
        let mut edges_inserted = 0usize;
        let mut edges_dropped_unresolvable = 0usize;
        let mut edges_dropped_self_loop = 0usize;
        let mut dropped_edges: Vec<DroppedEdgeDetail> = Vec::new();
        // Authoritative count of edges persisted with `relation_type = UNCLASSIFIED` (FR-005) —
        // taken here, not in the pre-lock strict-mode pass above, so an edge that was marked
        // reclassified but then dropped as self-referential or unresolvable is never counted.
        let mut edges_reclassified_unclassified = 0usize;

        // Apply dedup decisions → collect entity UUIDs
        let mut entity_uuids: Vec<String> = Vec::with_capacity(decisions.len());
        for decision in decisions {
            match decision {
                DedupDecision::Merge {
                    existing_uuid,
                    merged_summary,
                    summary_embedding,
                } => {
                    // `summary` and `summary_embedding` go in ONE statement (issue #651): the
                    // logged WAL template then names `$summary_embedding`, `log_mutation` strips
                    // the vector, and replay recomputes it from the co-located `summary`
                    // (ADR-0526) — so a rebuilt database embeds exactly what this one does.
                    if let Some(emb) = summary_embedding {
                        conn.exec_params(
                            "MATCH (e:Entity {uuid: $uuid}) \
                             SET e.summary = $summary, e.summary_embedding = $summary_embedding",
                            serde_json::json!({
                                "uuid": &existing_uuid,
                                "summary": &merged_summary,
                                "summary_embedding": emb,
                            }),
                        )?;
                    }
                    entity_uuids.push(existing_uuid);
                }
                DedupDecision::Insert { row } => {
                    let uuid = row.uuid.clone();
                    conn.insert_entity(&row)?;
                    entity_uuids.push(uuid);
                }
            }
        }

        // name→uuid map for edge endpoint resolution. Keys use `prompts::normalize_name` (control
        // -char strip + trim + lowercase) — the same normalization a name gets before it reaches
        // the model — so neither a batch-internal case mismatch (#209) nor a control character
        // in the original entity name causes a genuine batch-local match to fall through to the
        // global fallback unnecessarily.
        //
        // Multi-kind (issue #616): edges carry names only, so a name can map to one entity per
        // kind in this batch (e.g. a `Person` "Aurora" and a default-kind "Aurora"). Within one
        // kind a later duplicate replaces the earlier (the pre-#616 behaviour); across kinds the
        // name is *ambiguous* and the edge is dropped rather than guessed at (#414 / ADR-0615).
        let mut name_to_uuid: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for (i, e) in extraction.entities.iter().enumerate() {
            let slot = name_to_uuid.entry(normalize_name(&e.name)).or_default();
            match slot.iter_mut().find(|(k, _)| *k == entity_kinds[i]) {
                Some(existing) => existing.1 = entity_uuids[i].clone(),
                None => slot.push((entity_kinds[i].clone(), entity_uuids[i].clone())),
            }
        }

        let mut scan_cache: std::collections::HashMap<String, EndpointResolution> =
            std::collections::HashMap::new();
        let mut resolve_via_scan = |raw_name: &str| -> Result<EndpointResolution, Error> {
            let key = raw_name.trim().to_lowercase();
            if let Some(cached) = scan_cache.get(&key) {
                return Ok(cached.clone());
            }
            let resolution =
                resolve_stored_endpoint(&conn, raw_name, &gid_owned, &endpoint_kinds)?;
            scan_cache.insert(key, resolution.clone());
            Ok(resolution)
        };
        // Resolves one edge endpoint: this batch first, then the persisted graph.
        let mut resolve_endpoint = |raw_name: &str| -> Result<EndpointResolution, Error> {
            match name_to_uuid.get(&normalize_name(raw_name)).map(Vec::as_slice) {
                Some([(_, uuid)]) => Ok(EndpointResolution::Found(uuid.clone())),
                Some(several) if several.len() > 1 => Ok(EndpointResolution::Ambiguous),
                _ => resolve_via_scan(raw_name),
            }
        };

        // Insert relationship edges. This is the sole, authoritative point at which an edge's
        // endpoints are finally resolved or the edge is dropped (FR-003, FR-005) — pre-lock,
        // above, only salvage-rewrites an off-list endpoint name; it never drops for endpoint
        // reasons (except the always-correct self-referential case).
        for (i, edge) in extraction.edges.iter().enumerate() {
            // An endpoint absent from this batch's name→uuid map may still resolve against the
            // persisted Entity table (e.g. a recurring hub entity created in an earlier ingest
            // batch, or salvage-rewritten pre-lock above to a name this batch doesn't itself
            // contain) (FR-002, FR-003).
            //
            // Endpoint-authority resolution (issue #283/#221): a `lookup_key` miss here must not
            // be trusted as "doesn't exist" — see `get_entity_by_name_ci_with_scan_fallback`'s
            // doc comment. `resolve_via_scan` above bounds this loop to at most one scan per
            // unique unresolved name in the batch, for both hits (also self-healed via a
            // `lookup_key` write for future requests) and misses (memoized only for this pass,
            // since there's nothing to persist for a name that doesn't exist).
            let src_res = resolve_endpoint(&edge.source_name)?;
            let dst_res = resolve_endpoint(&edge.target_name)?;
            let ambiguous = src_res == EndpointResolution::Ambiguous
                || dst_res == EndpointResolution::Ambiguous;
            let src_uuid = src_res.into_uuid();
            let dst_uuid = dst_res.into_uuid();
            let (src_uuid, dst_uuid) = match (src_uuid, dst_uuid) {
                (Some(s), Some(d)) => (s, d),
                (src, dst) => {
                    if ambiguous {
                        eprintln!(
                            "liminis-context-graph: dropping edge at commit, ambiguous endpoint (same name under more than one kind): '{}' → '{}'",
                            edge.source_name, edge.target_name
                        );
                    }
                    eprintln!(
                        "liminis-context-graph: dropping edge at commit, unresolvable endpoint: '{}' → '{}' (src_resolved={}, dst_resolved={})",
                        edge.source_name, edge.target_name, src.is_some(), dst.is_some()
                    );
                    edges_dropped_unresolvable += 1;
                    let unresolved_endpoint = match (src.is_some(), dst.is_some()) {
                        (false, false) => UnresolvedEndpoint::Both,
                        (false, true) => UnresolvedEndpoint::Source,
                        (true, false) => UnresolvedEndpoint::Target,
                        (true, true) => unreachable!("both endpoints resolved is not a drop"),
                    };
                    dropped_edges.push(DroppedEdgeDetail {
                        source_name: edge.source_name.clone(),
                        target_name: edge.target_name.clone(),
                        relation_type: edge.relation_type.clone(),
                        fact: edge.fact.clone(),
                        unresolved_endpoint,
                    });
                    continue;
                }
            };
            // UUID-level self-loop guard (issue #666): two differently named endpoints can
            // resolve to one entity (dedup merges, salvage), which the name-level filters above
            // cannot see. Counted separately from unresolvable drops (ADR-0051's
            // one-`dropped_edges`-entry-per-unresolvable-drop contract is unchanged).
            if src_uuid == dst_uuid {
                eprintln!(
                    "liminis-context-graph: dropping edge at commit, endpoints resolve to the same entity ({}): '{}' → '{}'",
                    src_uuid, edge.source_name, edge.target_name
                );
                edges_dropped_self_loop += 1;
                continue;
            }
            conn.insert_relates_to_edge(&RelatesToEdge {
                ingested_at: String::new(),
                uuid: uuid::Uuid::new_v4().to_string(),
                name: format!("{} → {}", edge.source_name, edge.target_name),
                source_node_uuid: src_uuid,
                target_node_uuid: dst_uuid,
                group_id: gid_owned.clone(),
                fact: edge.fact.clone(),
                fact_embedding: fact_embeddings[i].clone(),
                created_at: ref_time_owned.clone(),
                valid_at: validate_llm_timestamp(edge.valid_at.clone())
                    .or_else(|| Some(ref_time_owned.clone())),
                invalid_at: validate_llm_timestamp(edge.invalid_at.clone()),
                attributes: match &edge.original_relation_type {
                    Some(orig) => {
                        serde_json::json!({ "original_relation_type": orig }).to_string()
                    }
                    None => "{}".to_string(),
                },
                relation_type: edge.relation_type.clone(),
                episode_uuids: vec![],
                source_descriptions: vec![],
            })?;
            edges_inserted += 1;
            // Open mode only reclassifies `extract: false` labels (#637), which always set
            // `original_relation_type`; a literal "UNCLASSIFIED" from the extractor never does.
            if (is_strict_mode || edge.original_relation_type.is_some())
                && edge.relation_type.as_deref() == Some(UNCLASSIFIED)
            {
                edges_reclassified_unclassified += 1;
            }
        }

        // Insert episodic node
        conn.insert_episodic(&EpisodicRow {
            ingested_at: String::new(),
            uuid: ep_uuid.clone(),
            name: name_owned,
            group_id: gid_owned.clone(),
            created_at: ref_time_owned.clone(),
            source: source_owned,
            source_description: source_desc_owned,
            content: body_owned,
            content_embedding,
            valid_at: ref_time_owned.clone(),
            entity_edges: entity_uuids.clone(),
            attributes: attributes_owned.clone(),
        })?;

        // Insert MENTIONS edges
        for entity_uuid in &entity_uuids {
            conn.insert_mentions_edge(&MentionsEdge {
                episodic_uuid: ep_uuid.clone(),
                entity_uuid: entity_uuid.clone(),
                group_id: gid_owned.clone(),
            })?;
        }

        let flushed = wal_exec::wal_flush_chunk(&state_c, &gid_wal, conn.drain_mutations());
        // Advance this group's persisted WAL position (issue #353, FR-002; made per-group by
        // issue #378; generation-scoped by issue #387) after the WAL flush, which itself runs
        // after every graph mutation above already committed individually (lbug auto-commits per
        // statement; this codebase reserves explicit transactions for replay's flush_batch
        // only). Writing here — strictly after both the graph commit and the WAL flush — is the
        // write-after-commit mechanism FR-003 requires: a crash before this point leaves
        // applied_seq trailing what's actually committed (safe, redoes a little work on resume),
        // never ahead of it (which would skip committed-but-unrecorded mutations). Non-fatal: a
        // missed write only means applied_seq stays stale, not that the chunk's mutations are
        // lost. The generation persisted alongside it is the writer's own cached value (read once
        // at construction, not a fresh disk read), so this highest-frequency write path pays no
        // extra filesystem I/O.
        if let Some((seq, generation)) = flushed {
            // issue #440 FR-007: this chunk's content_embedding/name_embedding values were just
            // computed by state_c's running embedder above — record that identity alongside
            // applied_seq/generation so embedding_model_status observes a live-ingest-only
            // group's identity too, not only a group that has been explicitly rebuilt.
            let embedding_identity = (state_c.embedding_model.as_str(), state_c.embedder.dim() as i64);
            if let Err(e) = conn.set_wal_position(
                &gid_wal,
                seq,
                generation.as_deref(),
                Some(embedding_identity),
            ) {
                eprintln!(
                    "liminis-context-graph: add_episode: failed to persist applied_seq={seq} (non-fatal): {e}"
                );
            }
        }

        Ok((
            edges_inserted,
            edges_dropped_unresolvable,
            edges_reclassified_unclassified,
            dropped_edges,
            edges_dropped_self_loop,
        ))
            },
        )
        .await??;
    // The write lock stays held through the sidecar/drift block below (issue #627): it makes the
    // `reloaded_since_extraction` check and the sidecar write atomic with respect to
    // `knowledge_reload_ontology`, which takes the same lock. Dropping it first would let a reload
    // land between the check and the write and have this episode's stale hash overwrite it.

    // After a successful DB commit, persist the current ontology hash to `.lcg/ontology-hash.json`
    // and clear the drift flag. Errors are non-fatal — a missed write means drift stays reported
    // until the next successful ingest.
    if let Some(ref root) = state.workspace_root {
        let ontology_ref = state.ontology.as_deref();
        if let Err(e) = ontology_sidecar::write_sidecar(root, ontology_ref) {
            eprintln!(
                "liminis-context-graph: ontology-sidecar: failed to update {:?}: {} — drift indicator may persist",
                root, e
            );
        } else if let Ok(mut guard) = state.ontology_drift.lock() {
            *guard = OntologyDriftState::default();
        }

        // Per-group clear (issue #451, FR-009): "Recreate + re-ingest" (the documented
        // remediation, User Story 5's own example) routes through add_episode, not just
        // handle_rebuild_from_wal — extend the clear to this group specifically, using the same
        // resolved ontology (`resolved_ontology`, Phase A above) that just guided this episode's
        // extraction, so the recorded hash matches what the DB now actually reflects.
        //
        // Skipped when a `knowledge_reload_ontology` (issue #627) swapped the group's cached
        // ontology between Phase A and here: recording this episode's (now stale) hash would
        // overwrite the reloaded ontology's sidecar and clear its drift. The episode's own
        // entities stay typed by the ontology it was extracted under.
        let reloaded_since_extraction = state
            .cached_ontology_hash(group_id)
            .is_some_and(|h| h != crate::ontology::content_hash(resolved_ontology.as_deref()));
        if reloaded_since_extraction {
            eprintln!(
                "liminis-context-graph: ontology-sidecar: group {group_id:?} ontology was reloaded during this episode's extraction — not recording the stale ontology hash"
            );
        } else if let Err(e) =
            ontology_sidecar::write_group_sidecar(root, group_id, resolved_ontology.as_deref())
        {
            eprintln!(
                "liminis-context-graph: ontology-sidecar: failed to update group sidecar for {:?}: {} — drift indicator may persist",
                group_id, e
            );
        } else {
            state.clear_group_drift(group_id, resolved_ontology.clone());
        }
    }
    drop(_write_guard);

    // Publish the ontology that guided this episode's extraction as a documentation-only sidecar
    // in the group's own WAL directory (FR-007) — travels automatically under the existing
    // whole-directory publish contract (see docs/operations.md). No lcg code path ever reads this
    // file back (FR-008): it can only ever inform a consumer inspecting the stream, never govern
    // their own extraction, validation, canonicalization, or reprocessing. Best-effort like the
    // workspace sidecar write above: a missed write only degrades documentation, never replay
    // (FR-009).
    if let Some(root) = state.wal_root.as_deref() {
        match crate::wal_group::group_wal_dir(root, group_id) {
            Ok(gid_dir) => {
                if let Err(e) = ontology_sidecar::write_wal_ontology_sidecar(&gid_dir, ontology_ref)
                {
                    eprintln!(
                        "liminis-context-graph: ontology-sidecar: failed to write published ontology sidecar for group {group_id:?} at {:?}: {} — documentation only, replay unaffected",
                        gid_dir, e
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "liminis-context-graph: ontology-sidecar: cannot resolve WAL directory for group {group_id:?}: {e} — skipping published ontology sidecar"
                );
            }
        }
    }

    Ok(AddEpisodeResult {
        episode_uuid,
        nodes_extracted,
        edges_extracted: edges_inserted,
        edges_dropped_unresolvable,
        edges_dropped_self_loop,
        dropped_edges,
        edges_reclassified_unclassified,
        entities_reclassified_unclassified,
        entities_dropped_malformed,
        edges_dropped_malformed,
        dedup_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-007: a genuine (non-missing-index) error from the hybrid dedup query must be
    /// distinguishable from a missing-index error, since `add_episode`'s retry logic only
    /// triggers `build_indices_once` when `is_missing_index_error` returns true — anything
    /// else takes the `Err(e) => return Err(e)` arm and propagates immediately, un-retried.
    ///
    /// `resolve_phase_b` is private, so this is exercised directly here rather than through
    /// the public `add_episode` API (there is no clean way to force a genuine DB error through
    /// the full ingest pipeline without reaching into internals — see #208 Plan).
    #[tokio::test]
    async fn resolve_phase_b_genuine_error_is_not_classified_as_missing_index() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("phase_b_error_test.db");
        let dim = 8;
        let db = Arc::new(crate::db::Db::open(db_path.to_str().unwrap()).unwrap());
        {
            let conn = db.connect().unwrap();
            conn.init_schema(dim).unwrap();
            // Build indices for real, so a subsequent failure here cannot be a missing-index
            // error — isolating the "genuine DB error" case FR-007 is about.
            conn.build_indices_and_constraints().unwrap();
        }

        // A name_embedding of the wrong dimension (dim+1 instead of dim) against an
        // already-indexed table is a genuine query failure, not a missing-index condition.
        let wrong_dim_embedding = vec![0.0f32; dim + 1];
        let result = resolve_phase_b(
            db,
            "test-group".to_string(),
            vec!["Someone".to_string()],
            vec![crate::types::DEFAULT_KIND.to_string()],
            vec![wrong_dim_embedding],
            true, // use_hybrid
        )
        .await;

        let err = match result {
            Ok(_) => panic!("dimension-mismatched vector query should fail"),
            Err(e) => e,
        };
        assert!(
            !is_missing_index_error(&err),
            "a dimension-mismatch error must not be classified as a missing-index error \
             (FR-007: only missing-index errors trigger auto-heal), got: {err}"
        );
    }
}
