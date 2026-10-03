use serde::{Deserialize, Serialize};

/// Classifies the origin format of an episode body for prompt dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceType {
    #[default]
    Text,
    Message,
    Json,
}

impl SourceType {
    /// Maps a string label to a SourceType; defaults to Text for unknown values.
    pub fn from_str_lossy(s: &str) -> Self {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("json") {
            Self::Json
        } else if trimmed.eq_ignore_ascii_case("message") {
            Self::Message
        } else {
            Self::Text
        }
    }
}

/// The default entity kind (issue #615, D1/D2). Every entity written before kinds existed, every
/// extraction-created entity, and every name-only direct write is this kind.
pub const DEFAULT_KIND: &str = "Entity";

/// The `lookup_key` field separator (U+001F). A `kind` may not contain it (FR-012), so a key
/// `group_id ␟ kind ␟ name` stays unambiguous.
pub const KEY_SEPARATOR: char = '\u{1f}';

/// Labels the code treats as structural markers on an `Entity` row. A kind is always added to
/// `labels`, so a kind equal to one of these would masquerade as the marker: `Merged` makes a live
/// entity look like a merge tombstone (unresolvable by name, skipped by counts and merges).
const RESERVED_KINDS: &[&str] = &["Merged"];

/// Validates and normalises a caller-supplied `kind` (issue #615 FR-012): trimmed, non-empty,
/// no U+001F, not a reserved structural label, case-sensitive. `None` means the default kind
/// [`DEFAULT_KIND`].
pub fn normalize_kind(kind: Option<&str>) -> Result<String, crate::error::Error> {
    match kind {
        None => Ok(DEFAULT_KIND.to_string()),
        Some(k) => {
            let t = k.trim();
            if t.is_empty() || t.contains(KEY_SEPARATOR) || RESERVED_KINDS.contains(&t) {
                return Err(crate::error::Error::InvalidKind(k.to_string()));
            }
            Ok(t.to_string())
        }
    }
}

/// One candidate of an ambiguous name-only resolution (issue #615 FR-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindCandidate {
    pub uuid: String,
    pub kind: String,
}

fn default_kind() -> String {
    DEFAULT_KIND.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRow {
    pub uuid: String,
    pub name: String,
    pub group_id: String,
    pub labels: Vec<String>,
    /// The entity's single identity-bearing kind (issue #615); always one of `labels`. Readers
    /// map a NULL column to [`DEFAULT_KIND`], so this is never empty on a row read from the db.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// LadybugDB TIMESTAMP as "YYYY-MM-DD HH:MM:SS".
    pub created_at: String,
    /// Service-clock time the record was first written (issue #673, ADR-0673) — the "knowledge
    /// time" clock, distinct from the caller-supplied event time in `created_at`/`valid_at`. Never
    /// caller-supplied: the `Conn::insert_*` helpers stamp it when empty. Same read-back format as
    /// `created_at` ("YYYY-MM-DD HH:MM:SS"); empty only for a NULL column.
    #[serde(default)]
    pub ingested_at: String,
    #[serde(skip)]
    pub name_embedding: Vec<f32>,
    pub summary: String,
    pub attributes: String,
    #[serde(default)]
    pub episode_uuids: Vec<String>,
    #[serde(default)]
    pub source_descriptions: Vec<String>,
    #[serde(skip)]
    pub summary_embedding: Vec<f32>,
}

impl Default for EntityRow {
    fn default() -> Self {
        Self {
            uuid: String::new(),
            name: String::new(),
            group_id: String::new(),
            labels: Vec::new(),
            kind: default_kind(),
            created_at: String::new(),
            ingested_at: String::new(),
            name_embedding: Vec::new(),
            summary: String::new(),
            attributes: String::new(),
            episode_uuids: Vec::new(),
            source_descriptions: Vec::new(),
            summary_embedding: Vec::new(),
        }
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn normalize_kind_defaults_trims_and_validates() {
        assert_eq!(normalize_kind(None).unwrap(), "Entity");
        assert_eq!(normalize_kind(Some(" Topic ")).unwrap(), "Topic");
        assert_eq!(normalize_kind(Some("topic")).unwrap(), "topic");
        assert!(normalize_kind(Some("")).is_err());
        assert!(normalize_kind(Some("   ")).is_err());
        assert!(normalize_kind(Some("a\u{1f}b")).is_err());
        assert!(normalize_kind(Some("Merged")).is_err());
        assert!(normalize_kind(Some(" Merged ")).is_err());
        assert!(normalize_kind(Some("merged")).is_ok());
    }

    #[test]
    fn entity_row_default_kind_is_entity() {
        assert_eq!(EntityRow::default().kind, "Entity");
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpisodicRow {
    pub uuid: String,
    pub name: String,
    pub group_id: String,
    pub created_at: String,
    pub source: String,
    pub source_description: String,
    pub content: String,
    #[serde(skip)]
    pub content_embedding: Vec<f32>,
    pub valid_at: String,
    /// Service-clock time the record was first written (issue #673, ADR-0673) — the "knowledge
    /// time" clock, distinct from the caller-supplied event time in `created_at`/`valid_at`. Never
    /// caller-supplied: the `Conn::insert_*` helpers stamp it when empty. Same read-back format as
    /// `created_at` ("YYYY-MM-DD HH:MM:SS"); empty only for a NULL column.
    #[serde(default)]
    pub ingested_at: String,
    pub entity_edges: Vec<String>,
    /// Caller-supplied structured metadata (issue #528), a JSON object serialized as a string.
    /// The write path (`episode::add_episode`) and the migration/WAL-rebuild zero-fill
    /// (`schema::zero_fill_null_episodic_attributes`) both maintain the invariant that this is a
    /// parseable JSON object string defaulting to `"{}"`, never absent or non-JSON — see
    /// ADR-0528. That zero-fill is best-effort and non-fatal on failure (deliberately, per
    /// ADR-0528 Decision 3 — no `SchemaState` retry marker, unlike `Entity.lookup_key`), so a row
    /// that survived a failed zero-fill attempt could still read back as `NULL` → `""` via
    /// `value_as_string`; callers parsing this field should tolerate that rather than assume the
    /// invariant is enforced unconditionally.
    pub attributes: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelatesToEdge {
    pub uuid: String,
    pub name: String,
    pub source_node_uuid: String,
    pub target_node_uuid: String,
    pub group_id: String,
    pub fact: String,
    #[serde(skip)]
    pub fact_embedding: Vec<f32>,
    pub created_at: String,
    /// Service-clock time the record was first written (issue #673, ADR-0673) — the "knowledge
    /// time" clock, distinct from the caller-supplied event time in `created_at`/`valid_at`. Never
    /// caller-supplied: the `Conn::insert_*` helpers stamp it when empty. Same read-back format as
    /// `created_at` ("YYYY-MM-DD HH:MM:SS"); empty only for a NULL column.
    #[serde(default)]
    pub ingested_at: String,
    pub valid_at: Option<String>,
    pub invalid_at: Option<String>,
    pub attributes: String,
    #[serde(default)]
    pub relation_type: Option<String>,
    /// Episodes mentioning either this edge's source or target entity (deduplicated),
    /// per ADR-0012's either-endpoint semantics — NOT evidence for this specific
    /// relationship. Populated only by read paths that call
    /// `enrich_edge_from_entity_ep_info` (`knowledge_list_relationships`,
    /// `knowledge_get_entity_neighbors`); other read paths (e.g.
    /// `knowledge_find_relationships`, `knowledge_get_edges_by_group`,
    /// `knowledge_get_edges_by_uuids`) leave this at its default empty vec.
    #[serde(default)]
    pub episode_uuids: Vec<String>,
    #[serde(default)]
    pub source_descriptions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MentionsEdge {
    pub episodic_uuid: String,
    pub entity_uuid: String,
    pub group_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub entities: Vec<ExtractedEntity>,
    pub edges: Vec<ExtractedEdge>,
}

/// Wraps [`ExtractionResult`] with the per-call malformed-item drop counts salvaged during
/// deserialization (#342 FR-001/FR-003). Kept as a separate wrapper rather than adding fields
/// directly to `ExtractionResult` — `ExtractionResult` is exhaustively literal-constructed at
/// dozens of test/fixture sites across the workspace, and this wrapper confines the new fields
/// to the `Extractor` trait boundary instead of forcing every one of those sites to change.
///
/// `#[serde(flatten)]` + `#[serde(default)]` on the two counters keeps the JSON shape backward
/// compatible with cassette records recorded before this change (`{"entities":[...],
/// "edges":[...]}`, with no drop counters) — those still deserialize, with both counts
/// defaulting to `0`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionOutcome {
    #[serde(flatten)]
    pub result: ExtractionResult,
    #[serde(default)]
    pub entities_dropped_malformed: usize,
    #[serde(default)]
    pub edges_dropped_malformed: usize,
}

impl From<ExtractionResult> for ExtractionOutcome {
    fn from(result: ExtractionResult) -> Self {
        Self {
            result,
            entities_dropped_malformed: 0,
            edges_dropped_malformed: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    pub entity_type: String,
    #[serde(default, deserialize_with = "deserialize_summary_or_default")]
    pub summary: String,
    /// The entity type as originally extracted, set only when the strict-mode filter
    /// reclassifies an out-of-vocabulary `entity_type` to `Unclassified` (FR-002/FR-003).
    /// `None` in every other case — including when the type normalizes to a declared type,
    /// which rewrites `entity_type` but does not touch this field, since the canonical name is
    /// not "lost" information the way an out-of-vocabulary label is.
    #[serde(default)]
    pub original_entity_type: Option<String>,
}

/// Deserializes `summary` as `""` when the field is absent or explicitly `null`, rather than
/// failing deserialization. Text-instructed (non-schema-enforced) OAI-compatible models may omit
/// this field even when the rest of the payload is well-formed; per issue #314, losing an entire
/// chunk's entities over one missing string field is a disproportionate trade.
fn deserialize_summary_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedEdge {
    pub source_name: String,
    pub target_name: String,
    pub fact: String,
    #[serde(default)]
    pub relation_type: Option<String>,
    #[serde(default)]
    pub valid_at: Option<String>,
    #[serde(default)]
    pub invalid_at: Option<String>,
    /// The relation type as originally extracted, set only when the strict-mode filter
    /// reclassifies an out-of-vocabulary `relation_type` to `UNCLASSIFIED` (FR-004). `None` in
    /// every other case — including when alias normalisation (FR-001) rewrites `relation_type`
    /// to a declared alias's canonical name, which does not touch this field, since the
    /// canonical name is not "lost" information the way an out-of-vocabulary label is.
    #[serde(default)]
    pub original_relation_type: Option<String>,
}

/// Which endpoint(s) of a dropped edge failed to resolve to an entity (issue #411 FR-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedEndpoint {
    Source,
    Target,
    Both,
}

/// An edge that was extracted but dropped at Phase C commit time because an endpoint never
/// resolved to an entity, counted in `AddEpisodeResult::edges_dropped_unresolvable` (issue #281).
/// Carries the edge's extracted content verbatim so a consumer can report what fact was lost and
/// why, without cross-referencing the persisted graph (issue #411 FR-001/FR-002/FR-003).
#[derive(Debug, Clone, Serialize)]
pub struct DroppedEdgeDetail {
    pub source_name: String,
    pub target_name: String,
    pub relation_type: Option<String>,
    pub fact: String,
    pub unresolved_endpoint: UnresolvedEndpoint,
}

/// A structurally valid (deserializes fine) item can still be semantically empty — e.g. a
/// `String` field that is present but blank or whitespace-only. `salvage_items` (extractor.rs)
/// checks this in addition to deserializability, per #347. The blankness test is
/// `str::trim().is_empty()`, matching `episode.rs`'s pre-existing empty-name `retain` exactly —
/// no new whitespace semantics are introduced.
pub(crate) trait RequiredFieldsPresent {
    fn is_well_formed(&self) -> bool;
}

impl RequiredFieldsPresent for ExtractedEntity {
    fn is_well_formed(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

impl RequiredFieldsPresent for ExtractedEdge {
    fn is_well_formed(&self) -> bool {
        !self.source_name.trim().is_empty()
            && !self.target_name.trim().is_empty()
            && !self.fact.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PassageResult {
    pub uuid: String,
    pub name: String,
    pub content: String,
    pub source_description: String,
    pub group_id: String,
    pub created_at: String,
    pub valid_at: Option<String>,
    pub score: f64,
    /// The originating episode's structured metadata (issue #528, FR-010) — a JSON object
    /// serialized as a string, sourced from `Episodic.attributes`. See ADR-0528.
    pub attributes: String,
}

/// Why an entity was returned by `knowledge_find_entities` (issue #629): the fused RRF score
/// plus each retrieval path's raw evidence. A path that did not retrieve the item reports
/// `None` (serialized as `null`). Similarities are cosine similarity (`1 - distance`); a
/// non-finite value is reported as `None` so it never reaches JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntitySearchEvidence {
    pub rrf_score: f64,
    /// `true` iff the BM25 (full-text) path retrieved the item.
    pub text_match: bool,
    pub bm25_score: Option<f64>,
    pub name_similarity: Option<f64>,
    pub summary_similarity: Option<f64>,
}

/// Why a fact was returned by `knowledge_find_relationships` (issue #629). See
/// [`EntitySearchEvidence`] for field semantics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EdgeSearchEvidence {
    pub rrf_score: f64,
    pub text_match: bool,
    pub bm25_score: Option<f64>,
    pub fact_similarity: Option<f64>,
}

/// An [`EntityRow`] with its `search` evidence, flattened so the row's own fields stay at the
/// top level. Used only by `knowledge_find_entities`; `EntityRow` itself is unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct ScoredEntity {
    #[serde(flatten)]
    pub entity: EntityRow,
    pub search: EntitySearchEvidence,
}

/// A [`RelatesToEdge`] with its `search` evidence, flattened. Used only by
/// `knowledge_find_relationships`.
#[derive(Debug, Clone, Serialize)]
pub struct ScoredEdge {
    #[serde(flatten)]
    pub edge: RelatesToEdge,
    pub search: EdgeSearchEvidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scored_entity_flattens_row_and_serializes_null_evidence() {
        let scored = ScoredEntity {
            entity: EntityRow {
                uuid: "u1".into(),
                name: "Apollo".into(),
                ..Default::default()
            },
            search: EntitySearchEvidence {
                rrf_score: 0.5,
                text_match: false,
                bm25_score: None,
                name_similarity: Some(0.75),
                summary_similarity: None,
            },
        };
        let v = serde_json::to_value(&scored).unwrap();
        assert_eq!(v["uuid"], "u1");
        assert_eq!(v["name"], "Apollo");
        assert_eq!(v["search"]["text_match"], false);
        assert!(v["search"]["bm25_score"].is_null());
        assert_eq!(v["search"]["name_similarity"], 0.75);
        assert!(v["search"]["summary_similarity"].is_null());
        assert!(v["search"].get("rrf_score").is_some());
    }

    #[test]
    fn scored_edge_flattens_edge_and_serializes_evidence() {
        let scored = ScoredEdge {
            edge: RelatesToEdge {
                uuid: "e1".into(),
                fact: "A knows B".into(),
                ..Default::default()
            },
            search: EdgeSearchEvidence {
                rrf_score: 0.03,
                text_match: true,
                bm25_score: Some(1.5),
                fact_similarity: None,
            },
        };
        let v = serde_json::to_value(&scored).unwrap();
        assert_eq!(v["uuid"], "e1");
        assert_eq!(v["fact"], "A knows B");
        assert_eq!(v["search"]["text_match"], true);
        assert_eq!(v["search"]["bm25_score"], 1.5);
        assert!(v["search"]["fact_similarity"].is_null());
    }

    #[test]
    fn extracted_entity_summary_absent_defaults_to_empty_string() {
        let entity: ExtractedEntity =
            serde_json::from_str(r#"{"name": "Apollo 11", "entity_type": "Mission"}"#).unwrap();
        assert_eq!(entity.summary, "");
    }

    #[test]
    fn extracted_entity_summary_null_defaults_to_empty_string() {
        let entity: ExtractedEntity = serde_json::from_str(
            r#"{"name": "Apollo 11", "entity_type": "Mission", "summary": null}"#,
        )
        .unwrap();
        assert_eq!(entity.summary, "");
    }

    #[test]
    fn extracted_entity_summary_explicit_empty_string_stays_empty() {
        let entity: ExtractedEntity = serde_json::from_str(
            r#"{"name": "Apollo 11", "entity_type": "Mission", "summary": ""}"#,
        )
        .unwrap();
        assert_eq!(entity.summary, "");
    }

    #[test]
    fn extracted_entity_summary_present_is_preserved() {
        let entity: ExtractedEntity = serde_json::from_str(
            r#"{"name": "Apollo 11", "entity_type": "Mission", "summary": "A NASA mission."}"#,
        )
        .unwrap();
        assert_eq!(entity.summary, "A NASA mission.");
    }

    #[test]
    fn extracted_entity_missing_name_still_fails() {
        let result: Result<ExtractedEntity, _> =
            serde_json::from_str(r#"{"entity_type": "Mission", "summary": "x"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn extraction_outcome_deserializes_pre_342_cassette_shape_with_zero_drops() {
        // A cassette record written before #342 has no drop-counter keys at all — confirms
        // the #[serde(flatten)] + #[serde(default)] shape stays replayable against pre-existing
        // recordings on disk.
        let outcome: ExtractionOutcome = serde_json::from_str(
            r#"{"entities": [{"name": "Alice", "entity_type": "Person"}], "edges": []}"#,
        )
        .unwrap();
        assert_eq!(outcome.result.entities.len(), 1);
        assert_eq!(outcome.entities_dropped_malformed, 0);
        assert_eq!(outcome.edges_dropped_malformed, 0);
    }

    #[test]
    fn extraction_outcome_round_trips_drop_counters() {
        let outcome = ExtractionOutcome {
            result: ExtractionResult::default(),
            entities_dropped_malformed: 2,
            edges_dropped_malformed: 1,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let round_tripped: ExtractionOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.entities_dropped_malformed, 2);
        assert_eq!(round_tripped.edges_dropped_malformed, 1);
    }

    fn valid_entity() -> ExtractedEntity {
        ExtractedEntity {
            name: "Apollo 11".to_string(),
            entity_type: "Mission".to_string(),
            summary: String::new(),
            original_entity_type: None,
        }
    }

    fn valid_edge() -> ExtractedEdge {
        ExtractedEdge {
            source_name: "Apollo 11".to_string(),
            target_name: "Moon".to_string(),
            fact: "Apollo 11 landed on the Moon".to_string(),
            relation_type: None,
            valid_at: None,
            invalid_at: None,
            original_relation_type: None,
        }
    }

    #[test]
    fn well_formed_entity_passes() {
        assert!(valid_entity().is_well_formed());
    }

    #[test]
    fn entity_blank_name_fails() {
        let entity = ExtractedEntity {
            name: "".to_string(),
            ..valid_entity()
        };
        assert!(!entity.is_well_formed());
    }

    #[test]
    fn entity_whitespace_only_name_fails() {
        let entity = ExtractedEntity {
            name: "   \t\n".to_string(),
            ..valid_entity()
        };
        assert!(!entity.is_well_formed());
    }

    #[test]
    fn well_formed_edge_passes() {
        assert!(valid_edge().is_well_formed());
    }

    #[test]
    fn edge_blank_fact_fails() {
        let edge = ExtractedEdge {
            fact: "".to_string(),
            ..valid_edge()
        };
        assert!(!edge.is_well_formed());
    }

    #[test]
    fn edge_whitespace_only_fact_fails() {
        let edge = ExtractedEdge {
            fact: "   ".to_string(),
            ..valid_edge()
        };
        assert!(!edge.is_well_formed());
    }

    #[test]
    fn edge_blank_source_name_fails() {
        let edge = ExtractedEdge {
            source_name: "".to_string(),
            ..valid_edge()
        };
        assert!(!edge.is_well_formed());
    }

    #[test]
    fn edge_blank_target_name_fails() {
        let edge = ExtractedEdge {
            target_name: "".to_string(),
            ..valid_edge()
        };
        assert!(!edge.is_well_formed());
    }

    #[test]
    fn edge_two_blank_fields_still_single_false() {
        // Both `fact` and `source_name` blank simultaneously — the predicate returns one
        // bool, so the caller can only ever count this as a single drop, never two.
        let edge = ExtractedEdge {
            source_name: "".to_string(),
            fact: "".to_string(),
            ..valid_edge()
        };
        assert!(!edge.is_well_formed());
    }
}
