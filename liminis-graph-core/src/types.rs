use serde::{Deserialize, Serialize};

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
