#[derive(Debug, Clone, Default)]
pub struct EntityRow {
    pub uuid: String,
    pub name: String,
    pub group_id: String,
    pub labels: Vec<String>,
    /// LadybugDB TIMESTAMP string, e.g. `"2026-01-01 00:00:00"`.
    pub created_at: String,
    /// Fixed-length embedding vector; length must match the schema dimension.
    pub name_embedding: Vec<f32>,
    pub summary: String,
    pub attributes: String,
}

#[derive(Debug, Clone, Default)]
pub struct EpisodicRow {
    pub uuid: String,
    pub name: String,
    pub group_id: String,
    /// LadybugDB TIMESTAMP string, e.g. `"2026-01-01 00:00:00"`.
    pub created_at: String,
    pub source: String,
    pub source_description: String,
    pub content: String,
    /// Fixed-length embedding vector; length must match the schema dimension.
    pub content_embedding: Vec<f32>,
    /// LadybugDB TIMESTAMP string, e.g. `"2026-01-01 00:00:00"`.
    pub valid_at: String,
    pub entity_edges: Vec<String>,
}
