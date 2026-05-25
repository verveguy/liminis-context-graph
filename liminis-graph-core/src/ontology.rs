use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ── Serde deserialization types (YAML schema) ─────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
struct OntologyFile {
    #[serde(default)]
    mode: Option<OntologyModeRaw>,
    #[serde(default)]
    entity_types: Vec<EntityTypeRaw>,
    #[serde(default)]
    relation_types: Vec<RelationTypeRaw>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OntologyModeRaw {
    Open,
    Strict,
}

#[derive(Debug, Clone, Deserialize)]
struct EntityTypeRaw {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RelationTypeRaw {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    source_type: Option<String>,
    #[serde(default)]
    target_type: Option<String>,
}

// ── Runtime types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OntologyMode {
    Open,
    Strict,
}

impl std::fmt::Display for OntologyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OntologyMode::Open => write!(f, "open"),
            OntologyMode::Strict => write!(f, "strict"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EntityTypeDef {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RelationTypeDef {
    pub name: String,
    pub description: Option<String>,
    pub source_type: Option<String>,
    pub target_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Ontology {
    pub mode: OntologyMode,
    pub entity_types: Vec<EntityTypeDef>,
    pub relation_types: Vec<RelationTypeDef>,
}

impl Ontology {
    pub fn has_entity_types(&self) -> bool {
        !self.entity_types.is_empty()
    }

    pub fn has_relation_types(&self) -> bool {
        !self.relation_types.is_empty()
    }

    pub fn entity_type_names(&self) -> HashSet<String> {
        self.entity_types.iter().map(|e| e.name.clone()).collect()
    }

    pub fn relation_type_names(&self) -> HashSet<String> {
        self.relation_types.iter().map(|r| r.name.clone()).collect()
    }
}

// ── Name normalization ────────────────────────────────────────────────────────

/// Normalizes an entity type name to PascalCase.
/// e.g. "peer_reviewed_paper" → "PeerReviewedPaper", "PERSON" → "Person"
pub fn normalize_entity_type(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Split on word boundaries: underscores, hyphens, spaces, and case transitions
    let words = split_words(s);
    words
        .into_iter()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect()
}

/// Normalizes a relation type name to SCREAMING_SNAKE_CASE.
/// e.g. "authored by" → "AUTHORED_BY", "AffiliatedWith" → "AFFILIATED_WITH"
pub fn normalize_relation_type(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let words = split_words(s);
    words
        .into_iter()
        .map(|w| w.to_uppercase())
        .collect::<Vec<_>>()
        .join("_")
}

/// Splits a string into words on underscores, hyphens, spaces, and CamelCase boundaries.
fn split_words(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    let mut i = 0;
    while i < len {
        let c = chars[i];
        if c == '_' || c == '-' || c == ' ' {
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
            i += 1;
            continue;
        }
        // CamelCase boundary: uppercase after lowercase, or uppercase before lowercase with preceding uppercase
        if c.is_uppercase() && !current.is_empty() {
            let prev_is_lower = current
                .chars()
                .last()
                .map(|ch| ch.is_lowercase())
                .unwrap_or(false);
            let next_is_lower = i + 1 < len && chars[i + 1].is_lowercase();
            if prev_is_lower
                || (next_is_lower
                    && current
                        .chars()
                        .last()
                        .map(|ch| ch.is_uppercase())
                        .unwrap_or(false))
            {
                words.push(current.clone());
                current.clear();
            }
        }
        current.push(c);
        i += 1;
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

// ── File loading ──────────────────────────────────────────────────────────────

fn ontology_file_path(workspace_root: &Path) -> Option<PathBuf> {
    let primary = workspace_root.join(".lcg").join("ontology.yaml");
    if primary.exists() {
        return Some(primary);
    }
    let fallback = workspace_root.join(".graphiti").join("ontology.yaml");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

/// Loads and parses the workspace ontology file.
///
/// Returns `None` if:
/// - `workspace_root` is `None`
/// - no ontology file exists at the expected paths
/// - the file is empty or declares no entity types and no relation types
/// - the file is malformed (logged as a warning; does not panic)
pub fn load_ontology(workspace_root: Option<&Path>) -> Option<Ontology> {
    let root = workspace_root?;
    let path = ontology_file_path(root)?;

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "liminis-graph: ontology: failed to read {:?}: {} — falling back to free-form extraction",
                path, e
            );
            return None;
        }
    };

    // An empty or whitespace-only file is intentionally "no ontology" — don't log a parse error.
    if text.trim().is_empty() {
        return None;
    }

    let file: OntologyFile = match serde_yaml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "liminis-graph: ontology: YAML parse error in {:?}: {} — falling back to free-form extraction",
                path, e
            );
            return None;
        }
    };

    let mode = match file.mode {
        Some(OntologyModeRaw::Strict) => OntologyMode::Strict,
        _ => OntologyMode::Open,
    };

    let entity_types: Vec<EntityTypeDef> = file
        .entity_types
        .into_iter()
        .filter_map(|raw| {
            let normalized = normalize_entity_type(&raw.name);
            if normalized.is_empty() {
                eprintln!("liminis-graph: ontology: skipping entity type with blank name");
                return None;
            }
            if normalized != raw.name {
                eprintln!(
                    "liminis-graph: ontology: entity type '{}' normalized to '{}'",
                    raw.name, normalized
                );
            }
            Some(EntityTypeDef {
                name: normalized,
                description: raw.description,
            })
        })
        .collect();

    let relation_types: Vec<RelationTypeDef> = file
        .relation_types
        .into_iter()
        .filter_map(|raw| {
            let normalized = normalize_relation_type(&raw.name);
            if normalized.is_empty() {
                eprintln!("liminis-graph: ontology: skipping relation type with blank name");
                return None;
            }
            if normalized != raw.name {
                eprintln!(
                    "liminis-graph: ontology: relation type '{}' normalized to '{}'",
                    raw.name, normalized
                );
            }
            let source_type = raw.source_type.map(|s| normalize_entity_type(&s));
            let target_type = raw.target_type.map(|s| normalize_entity_type(&s));
            Some(RelationTypeDef {
                name: normalized,
                description: raw.description,
                source_type,
                target_type,
            })
        })
        .collect();

    // Coerce to None if both axes are empty — prevents injecting empty prompt sections
    if entity_types.is_empty() && relation_types.is_empty() {
        return None;
    }

    eprintln!(
        "liminis-graph: ontology: loaded {} entity type(s), {} relation type(s), mode={} from {:?}",
        entity_types.len(),
        relation_types.len(),
        mode,
        path
    );

    Some(Ontology {
        mode,
        entity_types,
        relation_types,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_ontology(dir: &TempDir, content: &str) -> PathBuf {
        let lcg_dir = dir.path().join(".lcg");
        std::fs::create_dir_all(&lcg_dir).unwrap();
        let path = lcg_dir.join("ontology.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    // ── normalization ─────────────────────────────────────────────────────────

    #[test]
    fn normalize_entity_type_pascal_case() {
        assert_eq!(normalize_entity_type("person"), "Person");
        assert_eq!(normalize_entity_type("ORGANIZATION"), "Organization");
        assert_eq!(
            normalize_entity_type("peer_reviewed_paper"),
            "PeerReviewedPaper"
        );
        assert_eq!(normalize_entity_type("MyEntity"), "MyEntity");
        assert_eq!(normalize_entity_type(""), "");
    }

    #[test]
    fn normalize_relation_type_screaming_snake() {
        assert_eq!(normalize_relation_type("AUTHORED"), "AUTHORED");
        assert_eq!(
            normalize_relation_type("affiliated_with"),
            "AFFILIATED_WITH"
        );
        assert_eq!(normalize_relation_type("AffiliatedWith"), "AFFILIATED_WITH");
        assert_eq!(normalize_relation_type("cites"), "CITES");
        assert_eq!(normalize_relation_type(""), "");
    }

    // ── load_ontology ─────────────────────────────────────────────────────────

    #[test]
    fn load_ontology_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let result = load_ontology(Some(dir.path()));
        assert!(result.is_none());
    }

    #[test]
    fn load_ontology_none_workspace_root_returns_none() {
        let result = load_ontology(None);
        assert!(result.is_none());
    }

    #[test]
    fn load_ontology_empty_types_returns_none() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "mode: open\nentity_types: []\nrelation_types: []\n");
        let result = load_ontology(Some(dir.path()));
        assert!(result.is_none(), "empty ontology should return None");
    }

    #[test]
    fn load_ontology_malformed_yaml_returns_none_no_panic() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "not: valid: yaml: [{{\n");
        let result = load_ontology(Some(dir.path()));
        assert!(result.is_none(), "malformed YAML should return None");
    }

    #[test]
    fn load_ontology_valid_file_returns_some() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            r#"
mode: strict
entity_types:
  - name: Person
    description: A human individual
  - name: Organization
    description: A company or institution
  - name: Paper
relation_types:
  - name: AUTHORED
    description: A person authored a paper
    source_type: Person
    target_type: Paper
  - name: AFFILIATED_WITH
    source_type: Person
    target_type: Organization
"#,
        );
        let ontology = load_ontology(Some(dir.path())).expect("should load ontology");
        assert_eq!(ontology.mode, OntologyMode::Strict);
        assert_eq!(ontology.entity_types.len(), 3);
        assert_eq!(ontology.relation_types.len(), 2);
        assert!(ontology.has_entity_types());
        assert!(ontology.has_relation_types());
        let names = ontology.entity_type_names();
        assert!(names.contains("Person"));
        assert!(names.contains("Organization"));
        assert!(names.contains("Paper"));
        let rnames = ontology.relation_type_names();
        assert!(rnames.contains("AUTHORED"));
        assert!(rnames.contains("AFFILIATED_WITH"));
    }

    #[test]
    fn load_ontology_entity_types_only() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "mode: open\nentity_types:\n  - name: Person\n");
        let ontology = load_ontology(Some(dir.path())).expect("should load");
        assert_eq!(ontology.entity_types.len(), 1);
        assert_eq!(ontology.relation_types.len(), 0);
        assert!(!ontology.has_relation_types());
    }

    #[test]
    fn load_ontology_defaults_to_open_mode() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "entity_types:\n  - name: Concept\n");
        let ontology = load_ontology(Some(dir.path())).expect("should load");
        assert_eq!(ontology.mode, OntologyMode::Open);
    }

    #[test]
    fn load_ontology_normalizes_names() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: peer_reviewed_paper\nrelation_types:\n  - name: affiliated_with\n",
        );
        let ontology = load_ontology(Some(dir.path())).expect("should load");
        assert_eq!(ontology.entity_types[0].name, "PeerReviewedPaper");
        assert_eq!(ontology.relation_types[0].name, "AFFILIATED_WITH");
    }

    #[test]
    fn load_ontology_fallback_graphiti_path() {
        let dir = TempDir::new().unwrap();
        let graphiti_dir = dir.path().join(".graphiti");
        std::fs::create_dir_all(&graphiti_dir).unwrap();
        std::fs::write(
            graphiti_dir.join("ontology.yaml"),
            "entity_types:\n  - name: Person\n",
        )
        .unwrap();
        let ontology = load_ontology(Some(dir.path())).expect("should load from .graphiti");
        assert_eq!(ontology.entity_types.len(), 1);
    }
}
