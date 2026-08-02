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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityRow {
    pub uuid: String,
    pub name: String,
    pub group_id: String,
    pub labels: Vec<String>,
    /// LadybugDB TIMESTAMP as "YYYY-MM-DD HH:MM:SS".
    pub created_at: String,
    #[serde(skip)]
    pub name_embedding: Vec<f32>,
    pub summary: String,
    pub attributes: String,
    #[serde(default)]
    pub episode_uuids: Vec<String>,
    #[serde(default)]
    pub source_descriptions: Vec<String>,
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
    pub entity_edges: Vec<String>,
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
    pub valid_at: Option<String>,
    pub invalid_at: Option<String>,
    pub attributes: String,
    #[serde(default)]
    pub relation_type: Option<String>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
