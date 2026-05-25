use serde::{Deserialize, Serialize};

/// Source type of the episode being ingested. Selects the appropriate extraction prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    #[default]
    Text,
    Message,
    Json,
}

impl SourceType {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "message" => SourceType::Message,
            "json" => SourceType::Json,
            "text" => SourceType::Text,
            other => {
                eprintln!(
                    "liminis-graph: unknown source_type {:?}; falling back to Text",
                    other
                );
                SourceType::Text
            }
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
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedEdge {
    pub source_name: String,
    pub target_name: String,
    pub fact: String,
    #[serde(default)]
    pub relation_type: String,
    #[serde(default)]
    pub valid_at: Option<String>,
    #[serde(default)]
    pub invalid_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmbeddingResult {
    pub embedding: Vec<f32>,
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
