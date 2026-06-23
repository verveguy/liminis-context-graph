//! Relation canonicalization pass — maps free-text edge `name` to a controlled `relation_type`.
//!
//! Two-layer architecture (per spec #163):
//!   1. Lexical pass: stem/keyword rules driven from the workspace ontology.
//!   2. Embedding fallback (P2): cosine similarity against canonical type description glosses.
//!
//! Co-occurrence noise edges (`ALICE → BOB` pattern) are reclassified to UNCLASSIFIED and
//! WAL-recorded. They are NEVER deleted — 94% of arrow-named edges carry rich `relation_type`
//! values that would be irreversibly lost. See ADR-0054.
//! Unmatched residual edges are also marked `UNCLASSIFIED`.
//!
//! Crash mid-pass: only committed batches appear in the WAL. The pass is idempotent, so
//! re-running after recovery is safe — a second run adds zero new WAL mutations.
use std::collections::HashMap;
use std::sync::Arc;

use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    app_state::AppState,
    db::{cosine_similarity, value_as_string},
    error::Error,
    ontology::{normalize_relation_type, Ontology},
    wal_exec,
};

const DEFAULT_EMBEDDING_THRESHOLD: f32 = 0.7;
const PAGE_SIZE: usize = 500;
const WRITE_BATCH_SIZE: usize = 250;
const PROGRESS_EVERY: usize = 5000;

// ── Public types ──────────────────────────────────────────────────────────────

pub struct CanonicalizeParams {
    pub dry_run: bool,
    pub embedding_threshold: Option<f32>,
}

pub struct CanonicalizeReport {
    pub total_edges: usize,
    pub mapped_count: usize,
    pub noise_count: usize,
    pub residual_count: usize,
    pub embedding_fallback_promoted: usize,
    pub dry_run: bool,
}

impl CanonicalizeReport {
    fn to_json(&self) -> Value {
        let total = self.total_edges as f64;
        json!({
            "total_edges": self.total_edges,
            "mapped_count": self.mapped_count,
            "noise_count": self.noise_count,
            "residual_count": self.residual_count,
            "embedding_fallback_promoted": self.embedding_fallback_promoted,
            "mapped_pct": if total > 0.0 { (self.mapped_count as f64 / total * 100.0).round() } else { 0.0 },
            "noise_pct": if total > 0.0 { (self.noise_count as f64 / total * 100.0).round() } else { 0.0 },
            "residual_pct": if total > 0.0 { (self.residual_count as f64 / total * 100.0).round() } else { 0.0 },
            "dry_run": self.dry_run,
        })
    }
}

#[derive(Debug, Clone)]
pub enum EdgeClass {
    /// Edge maps to this canonical relation type.
    Mapped(String),
    /// Co-occurrence noise edge — reclassified to UNCLASSIFIED (never deleted; see ADR-0054).
    Noise,
    /// No lexical or embedding rule matched — mark UNCLASSIFIED.
    Residual,
}

#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub uuid: String,
    pub name: String,
    pub relation_type: Option<String>,
    pub fact: String,
}

// ── Lexical index ─────────────────────────────────────────────────────────────

pub struct LexicalIndex {
    /// SCREAMING_SNAKE_CASE token → canonical type name (exact match).
    exact: HashMap<String, String>,
    /// (lowercase keyword, canonical type name) for substring matching.
    keywords: Vec<(String, String)>,
    /// All canonical type names (for checking if current_rt is already canonical).
    canonical_names: std::collections::HashSet<String>,
}

pub fn build_lexical_index(ontology: &Ontology) -> LexicalIndex {
    let mut exact: HashMap<String, String> = HashMap::new();
    let mut keywords: Vec<(String, String)> = Vec::new();
    let mut canonical_names = std::collections::HashSet::new();

    for rt in &ontology.relation_types {
        canonical_names.insert(rt.name.clone());
        // Canonical name maps to itself
        exact.insert(rt.name.clone(), rt.name.clone());
        // Aliases map to the canonical name
        for alias in &rt.aliases {
            exact.insert(alias.clone(), rt.name.clone());
        }
        // Keywords for substring matching
        for kw in &rt.keywords {
            keywords.push((kw.clone(), rt.name.clone()));
        }
    }

    LexicalIndex {
        exact,
        keywords,
        canonical_names,
    }
}

// ── Noise detection ───────────────────────────────────────────────────────────

/// Returns true if `s` matches the co-occurrence noise pattern: `X → Y` or `X -> Y`
/// where both sides start with an uppercase letter (optionally followed by uppercase letters,
/// digits, and spaces). Bare lowercase words cannot match.
pub fn is_noise_edge(s: &str) -> bool {
    static NOISE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = NOISE_RE
        .get_or_init(|| Regex::new(r"^[A-Z][A-Z0-9 ]*\s*(→|->)\s*[A-Z][A-Z0-9 ]*$").unwrap());
    re.is_match(s.trim())
}

// ── Lexical classification ────────────────────────────────────────────────────

/// Classifies a single edge lexically using the ontology index.
///
/// Order of checks (FR-002: match against both `name` and `relation_type`):
/// 1. Noise pattern on `name` field → Noise
/// 2. Noise pattern on `relation_type` field → Noise
/// 3. Normalized `name` in exact map (canonical name or alias) → Mapped
/// 4. Any keyword matches a substring of the lowercase normalized name → Mapped
/// 5. `relation_type` is already a canonical name → Mapped (idempotent)
/// 6. Normalized `relation_type` in exact map (alias) or keyword match → Mapped
/// 7. → Residual
pub fn classify_edge_lexically(
    name: &str,
    current_rt: Option<&str>,
    idx: &LexicalIndex,
) -> EdgeClass {
    // Noise checks
    if is_noise_edge(name) {
        return EdgeClass::Noise;
    }
    if let Some(rt) = current_rt {
        if is_noise_edge(rt) {
            return EdgeClass::Noise;
        }
    }

    // Exact match on normalized name
    let normalized = normalize_relation_type(name);
    if let Some(canonical) = idx.exact.get(&normalized) {
        return EdgeClass::Mapped(canonical.clone());
    }

    // Keyword substring match on lowercase normalized name
    let lower = normalized.to_lowercase();
    for (kw, canonical) in &idx.keywords {
        if lower.contains(kw.as_str()) {
            return EdgeClass::Mapped(canonical.clone());
        }
    }

    // Secondary signal: check relation_type field (FR-002 — match on name *or* relation_type).
    // Covers edges where name is uninformative but relation_type was set to an alias by an
    // earlier pass, or the extractor used a variant that normalized to an alias form.
    if let Some(rt) = current_rt {
        if idx.canonical_names.contains(rt) {
            // Already canonical — idempotent (post-#94 ingestion path)
            return EdgeClass::Mapped(rt.to_string());
        }
        let rt_normalized = normalize_relation_type(rt);
        if let Some(canonical) = idx.exact.get(&rt_normalized) {
            return EdgeClass::Mapped(canonical.clone());
        }
        let rt_lower = rt_normalized.to_lowercase();
        for (kw, canonical) in &idx.keywords {
            if rt_lower.contains(kw.as_str()) {
                return EdgeClass::Mapped(canonical.clone());
            }
        }
    }

    EdgeClass::Residual
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Runs the canonicalization pass as a four-phase async operation.
///
/// Callers must add `knowledge_canonicalize_relations` to `service_protocol.py`
/// in the liminis-app repo.
pub async fn canonicalize_relations(
    state: Arc<AppState>,
    params: CanonicalizeParams,
    progress_tx: Option<UnboundedSender<Value>>,
    ontology: Arc<Ontology>,
) -> Result<Value, Error> {
    let threshold = params
        .embedding_threshold
        .unwrap_or(DEFAULT_EMBEDDING_THRESHOLD);

    // ── Phase A: paginated read of all RelatesToNode_ edges (read lock) ───────
    let db = state
        .db
        .load_full()
        .ok_or_else(|| Error::DbUnavailable("DB unavailable".to_string()))?;
    let _read_guard = state.write_lock.read().await;
    let db_a = Arc::clone(&db);
    let edges: Vec<EdgeRecord> = tokio::task::spawn_blocking(move || {
        let conn = db_a.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            let rows = conn
                .dump_relatos_page(None, offset, PAGE_SIZE)
                .map_err(|e| Error::Ipc(format!("read edges page: {e}")))?;
            let count = rows.len();
            for row in &rows {
                let uuid = value_as_string(&row[0]);
                let name = value_as_string(&row[1]);
                let fact = value_as_string(&row[4]);
                let rt_str = value_as_string(&row[11]);
                let relation_type = if rt_str.is_empty() {
                    None
                } else {
                    Some(rt_str)
                };
                all.push(EdgeRecord {
                    uuid,
                    name,
                    fact,
                    relation_type,
                });
            }
            if count < PAGE_SIZE {
                break;
            }
            offset += count;
        }
        Ok::<_, Error>(all)
    })
    .await??;
    drop(_read_guard);

    let total_edges = edges.len();

    // ── Phase B: lexical classification ───────────────────────────────────────
    let idx = build_lexical_index(&ontology);
    let mut classifications: Vec<(EdgeRecord, EdgeClass)> = Vec::with_capacity(edges.len());
    for (i, edge) in edges.into_iter().enumerate() {
        if i % PROGRESS_EVERY == 0 && i > 0 {
            if let Some(ref tx) = progress_tx {
                let _ = tx.send(json!({
                    "type": "progress",
                    "processed": i,
                    "total": total_edges,
                    "phase": "lexical",
                }));
            }
        }
        let class = classify_edge_lexically(&edge.name, edge.relation_type.as_deref(), &idx);
        classifications.push((edge, class));
    }

    // Separate residuals for embedding fallback
    let mut residuals: Vec<usize> = Vec::new();
    for (i, (_, class)) in classifications.iter().enumerate() {
        if matches!(class, EdgeClass::Residual) {
            residuals.push(i);
        }
    }

    // ── Phase C: embedding fallback for residual edges (async, no lock) ───────
    let mut embedding_fallback_promoted = 0usize;
    let mut embedding_skip_warning: Option<String> = None;

    // Pre-embed canonical type descriptions (one per type, cached for this call)
    let gloss_embeddings: Vec<(String, Vec<f32>)> = {
        let mut result = Vec::new();
        let types_with_desc: Vec<(String, String)> = ontology
            .relation_types
            .iter()
            .filter_map(|rt| {
                rt.description
                    .as_deref()
                    .filter(|d| !d.is_empty())
                    .map(|d| (rt.name.clone(), d.to_string()))
            })
            .collect();

        if types_with_desc.is_empty() {
            embedding_skip_warning = Some(
                "embedding fallback skipped: no canonical types have description glosses"
                    .to_string(),
            );
        } else {
            for (name, desc) in &types_with_desc {
                match state.embedder.embed(desc).await {
                    Ok(emb) => result.push((name.clone(), emb)),
                    Err(e) => {
                        embedding_skip_warning =
                            Some(format!("embedding fallback skipped: embedder error: {e}"));
                        result.clear();
                        break;
                    }
                }
            }
        }
        result
    };

    if !gloss_embeddings.is_empty() && !residuals.is_empty() {
        let total_residuals = residuals.len();
        for (ri, &idx_in_classifications) in residuals.iter().enumerate() {
            if ri % PROGRESS_EVERY == 0 && ri > 0 {
                if let Some(ref tx) = progress_tx {
                    let _ = tx.send(json!({
                        "type": "progress",
                        "processed": ri,
                        "total": total_residuals,
                        "phase": "embedding_fallback",
                    }));
                }
            }
            let fact = classifications[idx_in_classifications].0.fact.clone();
            if fact.is_empty() {
                continue;
            }
            match state.embedder.embed(&fact).await {
                Ok(fact_emb) => {
                    let best = gloss_embeddings
                        .iter()
                        .map(|(name, emb)| (name, cosine_similarity(&fact_emb, emb)))
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                    if let Some((canonical, sim)) = best {
                        if sim >= threshold {
                            classifications[idx_in_classifications].1 =
                                EdgeClass::Mapped(canonical.clone());
                            embedding_fallback_promoted += 1;
                        }
                    }
                }
                Err(_) => {
                    // Non-fatal: leave as Residual if embed fails for this edge
                }
            }
        }
    }

    // ── Count classes ─────────────────────────────────────────────────────────
    let mut mapped_count = 0usize;
    let mut noise_count = 0usize;
    let mut residual_count = 0usize;
    for (_, class) in &classifications {
        match class {
            EdgeClass::Mapped(_) => mapped_count += 1,
            EdgeClass::Noise => noise_count += 1,
            EdgeClass::Residual => residual_count += 1,
        }
    }

    let report = CanonicalizeReport {
        total_edges,
        mapped_count,
        noise_count,
        residual_count,
        embedding_fallback_promoted,
        dry_run: params.dry_run,
    };

    // Dry-run: return coverage report without mutations
    if params.dry_run {
        return Ok(report.to_json());
    }

    // ── Phase D: batched write lock — apply mutations in chunks of WRITE_BATCH_SIZE ──
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": mapped_count + noise_count + residual_count,
        }));
    }

    for batch in classifications.chunks(WRITE_BATCH_SIZE) {
        let batch_data: Vec<(String, Option<String>, EdgeClass)> = batch
            .iter()
            .map(|(edge, class)| (edge.uuid.clone(), edge.relation_type.clone(), class.clone()))
            .collect();

        let db_d = Arc::clone(&db);
        let wal_writer = Arc::clone(&state.wal_writer);
        let sink = Arc::clone(&state.sink);
        let _write_guard = state.write_lock.write().await;
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let conn = db_d.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            let mut exec_result: Result<(), Error> = Ok(());
            for (uuid, current_rt, class) in &batch_data {
                let res = match class {
                    EdgeClass::Mapped(canonical) => {
                        // Idempotency: skip if already set to the target value
                        if current_rt.as_deref() == Some(canonical.as_str()) {
                            Ok(())
                        } else {
                            conn.exec_params(
                                "MATCH (n:RelatesToNode_ {uuid: $uuid}) SET n.relation_type = $rt",
                                json!({ "uuid": uuid, "rt": canonical }),
                            )
                        }
                    }
                    EdgeClass::Residual => {
                        // Idempotency: skip if already UNCLASSIFIED
                        if current_rt.as_deref() == Some("UNCLASSIFIED") {
                            Ok(())
                        } else {
                            conn.exec_params(
                                "MATCH (n:RelatesToNode_ {uuid: $uuid}) SET n.relation_type = $rt",
                                json!({ "uuid": uuid, "rt": "UNCLASSIFIED" }),
                            )
                        }
                    }
                    EdgeClass::Noise => {
                        // ADR-0054: never delete noise edges — reclassify to UNCLASSIFIED.
                        // Idempotency: skip if already set (second run adds zero WAL mutations).
                        if current_rt.as_deref() == Some("UNCLASSIFIED") {
                            Ok(())
                        } else {
                            conn.exec_params(
                                "MATCH (n:RelatesToNode_ {uuid: $uuid}) SET n.relation_type = $rt",
                                json!({ "uuid": uuid, "rt": "UNCLASSIFIED" }),
                            )
                        }
                    }
                };
                if res.is_err() {
                    exec_result = res;
                    break;
                }
            }
            // Always flush whatever succeeded before any failure — the DB already committed
            // those mutations; skipping the WAL flush would leave DB/WAL out of sync.
            wal_exec::wal_flush_ungrouped(&wal_writer, conn.drain_mutations(), &sink);
            exec_result
        })
        .await??;
        drop(_write_guard);
    }

    let mut resp = report.to_json();
    if let Some(warning) = embedding_skip_warning {
        resp["embedding_warning"] = json!(warning);
    }
    Ok(resp)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::{EntityTypeDef, Ontology, OntologyMode, RelationTypeDef};

    fn make_ontology_with_rules() -> Ontology {
        Ontology {
            mode: OntologyMode::Open,
            entity_types: vec![EntityTypeDef {
                name: "Person".to_string(),
                description: None,
                parent: None,
            }],
            ancestor_map: std::collections::HashMap::new(),
            relation_types: vec![
                RelationTypeDef {
                    name: "AUTHORED".to_string(),
                    description: Some("a person authored something".to_string()),
                    source_type: None,
                    target_type: None,
                    aliases: vec!["WROTE".to_string(), "AUTHORED_BY".to_string()],
                    keywords: vec!["author".to_string(), "writ".to_string()],
                },
                RelationTypeDef {
                    name: "AFFILIATED_WITH".to_string(),
                    description: None,
                    source_type: None,
                    target_type: None,
                    aliases: vec!["WORKS_FOR".to_string()],
                    keywords: vec!["affiliat".to_string(), "employ".to_string()],
                },
            ],
        }
    }

    // ── Noise regex tests ─────────────────────────────────────────────────────

    #[test]
    fn test_noise_regex_precision() {
        // Valid noise patterns
        assert!(is_noise_edge("BRETT → RAJI"));
        assert!(is_noise_edge("BRETT -> RAJI"));
        assert!(is_noise_edge("ALICE SMITH → BOB JONES"));
        assert!(is_noise_edge("A → B"));
        assert!(is_noise_edge("A1 → B2"));
        assert!(is_noise_edge("FOO → BAR"));

        // Must NOT match
        assert!(!is_noise_edge("wrote"));
        assert!(!is_noise_edge("authored by"));
        assert!(!is_noise_edge("brett → raji")); // lowercase
        assert!(!is_noise_edge("Brett → Raji")); // mixed case (lowercase second char)
        assert!(!is_noise_edge("AFFILIATED_WITH"));
        assert!(!is_noise_edge("AUTHORED"));
        assert!(!is_noise_edge(""));
        assert!(!is_noise_edge("A → b")); // right side lowercase start
        assert!(!is_noise_edge("a → B")); // left side lowercase start
    }

    // ── LexicalIndex alias match ──────────────────────────────────────────────

    #[test]
    fn test_lexical_index_alias_match() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        // Alias "WROTE" maps to AUTHORED
        let class = classify_edge_lexically("WROTE", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED), got {class:?}"),
        }

        // Alias "AUTHORED_BY" maps to AUTHORED
        let class = classify_edge_lexically("AUTHORED_BY", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED)"),
        }

        // Alias "WORKS_FOR" maps to AFFILIATED_WITH
        let class = classify_edge_lexically("WORKS_FOR", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AFFILIATED_WITH"),
            _ => panic!("expected Mapped(AFFILIATED_WITH)"),
        }
    }

    // ── LexicalIndex keyword match ────────────────────────────────────────────

    #[test]
    fn test_lexical_index_keyword_match() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        // "authoring" normalizes to "AUTHORING", lowercase "authoring" contains "author"
        let class = classify_edge_lexically("authoring", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED), got {class:?}"),
        }

        // "was_employed_by" contains "employ" keyword
        let class = classify_edge_lexically("was_employed_by", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AFFILIATED_WITH"),
            _ => panic!("expected Mapped(AFFILIATED_WITH)"),
        }
    }

    // ── Already canonical edge — idempotent ───────────────────────────────────

    #[test]
    fn test_classify_already_canonical_idempotent() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        // Edge has name="unknown_thing" but relation_type="AUTHORED" (set by post-#94 ingestion)
        let class = classify_edge_lexically("unknown_thing", Some("AUTHORED"), &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED) for already-canonical relation_type"),
        }
    }

    // ── Residual edge ─────────────────────────────────────────────────────────

    #[test]
    fn test_classify_residual() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        let class = classify_edge_lexically("IS_THE_THIRD_COUSIN_TWICE_REMOVED_OF", None, &idx);
        assert!(matches!(class, EdgeClass::Residual));
    }

    // ── Noise class ───────────────────────────────────────────────────────────

    #[test]
    fn test_classify_noise() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        let class = classify_edge_lexically("BRETT → RAJI", None, &idx);
        assert!(matches!(class, EdgeClass::Noise));

        let class = classify_edge_lexically("FOO -> BAR", None, &idx);
        assert!(matches!(class, EdgeClass::Noise));
    }

    // ── Canonical name maps to itself ─────────────────────────────────────────

    #[test]
    fn test_canonical_name_maps_to_itself() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        let class = classify_edge_lexically("AUTHORED", None, &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED)"),
        }
    }

    // ── relation_type alias/keyword as secondary signal (FR-002) ─────────────

    #[test]
    fn test_classify_via_relation_type_alias() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        // name has no signal; relation_type is an alias → should be Mapped via secondary path
        let class = classify_edge_lexically("SOME_UNRECOGNIZED_PREDICATE", Some("WROTE"), &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AUTHORED"),
            _ => panic!("expected Mapped(AUTHORED) via relation_type alias, got {class:?}"),
        }
    }

    #[test]
    fn test_classify_via_relation_type_keyword() {
        let ontology = make_ontology_with_rules();
        let idx = build_lexical_index(&ontology);

        // name has no signal; relation_type contains a keyword when normalized/lowercased
        let class =
            classify_edge_lexically("SOME_UNRECOGNIZED_PREDICATE", Some("WAS_EMPLOYED_BY"), &idx);
        match class {
            EdgeClass::Mapped(t) => assert_eq!(t, "AFFILIATED_WITH"),
            _ => {
                panic!("expected Mapped(AFFILIATED_WITH) via relation_type keyword, got {class:?}")
            }
        }
    }
}
