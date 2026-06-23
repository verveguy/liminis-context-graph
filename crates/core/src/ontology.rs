use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

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
    #[serde(default)]
    parent: Option<String>,
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
    /// Exact SCREAMING_SNAKE_CASE name variants that map to this canonical type.
    #[serde(default)]
    aliases: Option<Vec<String>>,
    /// Lowercase substring keywords — if any keyword appears in the normalized name, maps here.
    #[serde(default)]
    keywords: Option<Vec<String>>,
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
    pub parent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RelationTypeDef {
    pub name: String,
    pub description: Option<String>,
    pub source_type: Option<String>,
    pub target_type: Option<String>,
    /// Exact SCREAMING_SNAKE_CASE name variants that map to this canonical type.
    pub aliases: Vec<String>,
    /// Lowercase substring keywords for fuzzy name matching.
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Ontology {
    pub mode: OntologyMode,
    pub entity_types: Vec<EntityTypeDef>,
    pub relation_types: Vec<RelationTypeDef>,
    /// Maps each entity type name to its ordered ancestor list (supertype-first, excluding
    /// the implicit `Entity` root). Types with no declared parent map to an empty vec.
    /// Precomputed at load time for O(1) lookup during bulk label stamping.
    pub ancestor_map: HashMap<String, Vec<String>>,
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
                "liminis-context-graph: ontology: failed to read {:?}: {} — falling back to free-form extraction",
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
                "liminis-context-graph: ontology: YAML parse error in {:?}: {} — falling back to free-form extraction",
                path, e
            );
            return None;
        }
    };

    let mode = match file.mode {
        Some(OntologyModeRaw::Strict) => OntologyMode::Strict,
        _ => OntologyMode::Open,
    };

    let mut entity_types: Vec<EntityTypeDef> = file
        .entity_types
        .into_iter()
        .filter_map(|raw| {
            let normalized = normalize_entity_type(&raw.name);
            if normalized.is_empty() {
                eprintln!("liminis-context-graph: ontology: skipping entity type with blank name");
                return None;
            }
            if normalized == "Entity" {
                eprintln!(
                    "liminis-context-graph: ontology: 'Entity' is the implicit root type and should not be declared in entity_types — ignoring"
                );
                return None;
            }
            if normalized != raw.name {
                eprintln!(
                    "liminis-context-graph: ontology: entity type '{}' normalized to '{}'",
                    raw.name, normalized
                );
            }
            let parent = raw.parent.map(|p| normalize_entity_type(&p));
            Some(EntityTypeDef {
                name: normalized,
                description: raw.description,
                parent,
            })
        })
        .collect();

    validate_and_clean_parents(&mut entity_types);
    let ancestor_map = compute_ancestor_map(&entity_types);

    let relation_types: Vec<RelationTypeDef> = file
        .relation_types
        .into_iter()
        .filter_map(|raw| {
            let normalized = normalize_relation_type(&raw.name);
            if normalized.is_empty() {
                eprintln!(
                    "liminis-context-graph: ontology: skipping relation type with blank name"
                );
                return None;
            }
            if normalized != raw.name {
                eprintln!(
                    "liminis-context-graph: ontology: relation type '{}' normalized to '{}'",
                    raw.name, normalized
                );
            }
            let source_type = raw.source_type.map(|s| normalize_entity_type(&s));
            let target_type = raw.target_type.map(|s| normalize_entity_type(&s));
            let aliases: Vec<String> = raw
                .aliases
                .unwrap_or_default()
                .into_iter()
                .map(|a| normalize_relation_type(&a))
                .filter(|a| !a.is_empty())
                .collect();
            let keywords: Vec<String> = raw
                .keywords
                .unwrap_or_default()
                .into_iter()
                .map(|k| k.to_lowercase())
                .filter(|k| !k.is_empty())
                .collect();
            Some(RelationTypeDef {
                name: normalized,
                description: raw.description,
                source_type,
                target_type,
                aliases,
                keywords,
            })
        })
        .collect();

    // Coerce to None if both axes are empty — prevents injecting empty prompt sections
    if entity_types.is_empty() && relation_types.is_empty() {
        return None;
    }

    eprintln!(
        "liminis-context-graph: ontology: loaded {} entity type(s), {} relation type(s), mode={} from {:?}",
        entity_types.len(),
        relation_types.len(),
        mode,
        path
    );

    Some(Ontology {
        mode,
        entity_types,
        relation_types,
        ancestor_map,
    })
}

// ── Parent validation and ancestor map ───────────────────────────────────────

/// Validates and cleans parent relationships in the entity type list in place.
///
/// Phase 1: any `parent` that names a type not in the declared set is cleared with a warning.
/// Phase 2: cycles (A → B → A, A → B → C → A, etc.) are detected by path-following; all
/// members of a cycle have their `parent` cleared and a single warning is logged.
pub fn validate_and_clean_parents(entity_types: &mut Vec<EntityTypeDef>) {
    let declared: HashSet<String> = entity_types.iter().map(|e| e.name.clone()).collect();

    // Phase 1: clear undeclared parents
    for et in entity_types.iter_mut() {
        if let Some(ref p) = et.parent {
            if !declared.contains(p.as_str()) {
                eprintln!(
                    "liminis-context-graph: ontology: entity type '{}' declares parent '{}' \
                     which is not in the declared type set — treating as no parent",
                    et.name, p
                );
                et.parent = None;
            }
        }
    }

    // Phase 2: detect cycles by following each node's parent chain.
    // Use String keys so the map does not borrow from entity_types, allowing mutation below.
    let index_of: HashMap<String, usize> = entity_types
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.clone(), i))
        .collect();

    // Track which nodes we've already processed (no cycle reachable from them)
    let mut safe: HashSet<usize> = HashSet::new();

    for start in 0..entity_types.len() {
        if safe.contains(&start) {
            continue;
        }

        // Walk the parent chain from `start`, collecting the path
        let mut path: Vec<usize> = Vec::new();
        let mut visited_in_path: HashMap<usize, usize> = HashMap::new(); // index → position in path

        let mut cur = start;
        loop {
            if safe.contains(&cur) {
                // Entire path is cycle-free
                safe.extend(path.iter().copied());
                break;
            }
            if let Some(&cycle_start_pos) = visited_in_path.get(&cur) {
                // Found a cycle: path[cycle_start_pos..] forms the cycle
                let cycle: Vec<usize> = path[cycle_start_pos..].to_vec();
                // Collect names as owned Strings before mutating the vec
                let names: Vec<String> = cycle
                    .iter()
                    .map(|&i| entity_types[i].name.clone())
                    .collect();
                eprintln!(
                    "liminis-context-graph: ontology: cycle detected in entity type parent graph: {} — \
                     clearing parents for all cycle members",
                    names.join(" → ")
                );
                for &i in &cycle {
                    entity_types[i].parent = None;
                }
                // Nodes before the cycle entry are safe (they lead into the cycle, which is now broken)
                for &i in &path[..cycle_start_pos] {
                    safe.insert(i);
                }
                break;
            }

            visited_in_path.insert(cur, path.len());
            path.push(cur);

            // Advance to parent; read parent as an owned String to avoid holding a borrow across mutation
            let next_idx = entity_types[cur]
                .parent
                .as_deref()
                .and_then(|p| index_of.get(p))
                .copied();
            match next_idx {
                Some(next) => cur = next,
                None => {
                    // Reached a root (no parent or parent already cleared)
                    safe.extend(path.iter().copied());
                    break;
                }
            }
        }
    }
}

/// Precomputes the ancestor map for the given (validated, cycle-free) entity type list.
///
/// Returns a `HashMap` from type name to ordered ancestor list (supertype-first, excluding
/// the implicit `Entity` root). Types with no parent map to an empty vec.
pub fn compute_ancestor_map(entity_types: &[EntityTypeDef]) -> HashMap<String, Vec<String>> {
    let parent_of: HashMap<&str, &str> = entity_types
        .iter()
        .filter_map(|e| e.parent.as_deref().map(|p| (e.name.as_str(), p)))
        .collect();

    let mut map = HashMap::new();
    for et in entity_types {
        let mut ancestors = Vec::new();
        let mut cur = et.name.as_str();
        while let Some(&p) = parent_of.get(cur) {
            ancestors.push(p.to_string());
            cur = p;
        }
        ancestors.reverse(); // supertype-first (root at index 0)
        map.insert(et.name.clone(), ancestors);
    }
    map
}

// ── Content hash ─────────────────────────────────────────────────────────────

/// Returns a stable semantic content hash of the given ontology.
///
/// - `None` returns the sentinel string `"none"`, representing "ingested with no ontology".
/// - Hash is based on the parsed struct, not raw YAML bytes — cosmetic edits (whitespace,
///   comments) produce the same hash.
/// - Canonical form: `"mode:{mode}\nentity_types:{entries}\nrelation_types:{entries}"` where
///   entries are sorted by name and formatted as `NAME\0DESCRIPTION` (entity) or
///   `NAME\0SOURCE\0TARGET\0DESCRIPTION` (relation), joined with `\0\0`.
pub fn content_hash(ontology: Option<&Ontology>) -> String {
    let Some(o) = ontology else {
        return "none".to_string();
    };

    let mut entity_entries: Vec<String> = o
        .entity_types
        .iter()
        .map(|e| format!("{}\0{}", e.name, e.description.as_deref().unwrap_or("")))
        .collect();
    entity_entries.sort_unstable();

    let mut relation_entries: Vec<String> = o
        .relation_types
        .iter()
        .map(|r| {
            let mut aliases = r.aliases.clone();
            aliases.sort_unstable();
            let mut keywords = r.keywords.clone();
            keywords.sort_unstable();
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}",
                r.name,
                r.source_type.as_deref().unwrap_or(""),
                r.target_type.as_deref().unwrap_or(""),
                r.description.as_deref().unwrap_or(""),
                aliases.join("\0"),
                keywords.join("\0"),
            )
        })
        .collect();
    relation_entries.sort_unstable();

    // Collect parent edges and append them only when at least one parent is declared.
    // This preserves hash backward compatibility for flat ontologies (no spurious drift
    // on upgrade from pre-#173 versions). See ADR-0053.
    let mut parent_entries: Vec<String> = o
        .entity_types
        .iter()
        .filter_map(|e| e.parent.as_deref().map(|p| format!("{}\0{}", e.name, p)))
        .collect();
    parent_entries.sort_unstable();

    let canonical = if parent_entries.is_empty() {
        format!(
            "mode:{}\nentity_types:{}\nrelation_types:{}",
            o.mode,
            entity_entries.join("\0\0"),
            relation_entries.join("\0\0"),
        )
    } else {
        format!(
            "mode:{}\nentity_types:{}\nrelation_types:{}\nparent_edges:{}",
            o.mode,
            entity_entries.join("\0\0"),
            relation_entries.join("\0\0"),
            parent_entries.join("\0\0"),
        )
    };

    let digest = Sha256::digest(canonical.as_bytes());
    format!("{:x}", digest)
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

    // ── content_hash ─────────────────────────────────────────────────────────

    #[allow(clippy::type_complexity)]
    fn make_ontology(
        mode: OntologyMode,
        entities: &[(&str, Option<&str>)],
        relations: &[(&str, Option<&str>, Option<&str>, Option<&str>)],
    ) -> Ontology {
        let entity_types: Vec<EntityTypeDef> = entities
            .iter()
            .map(|(name, desc)| EntityTypeDef {
                name: name.to_string(),
                description: desc.map(|s| s.to_string()),
                parent: None,
            })
            .collect();
        let ancestor_map = compute_ancestor_map(&entity_types);
        Ontology {
            mode,
            entity_types,
            ancestor_map,
            relation_types: relations
                .iter()
                .map(|(name, src, tgt, desc)| crate::ontology::RelationTypeDef {
                    name: name.to_string(),
                    source_type: src.map(|s| s.to_string()),
                    target_type: tgt.map(|s| s.to_string()),
                    description: desc.map(|s| s.to_string()),
                    aliases: vec![],
                    keywords: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn content_hash_none_returns_sentinel() {
        assert_eq!(content_hash(None), "none");
    }

    #[test]
    fn content_hash_same_ontology_is_stable() {
        let o = make_ontology(OntologyMode::Open, &[("Person", None)], &[]);
        let h1 = content_hash(Some(&o));
        let h2 = content_hash(Some(&o));
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_entity_addition_changes_hash() {
        let o1 = make_ontology(OntologyMode::Open, &[("Person", None)], &[]);
        let o2 = make_ontology(
            OntologyMode::Open,
            &[("Person", None), ("Equipment", None)],
            &[],
        );
        assert_ne!(content_hash(Some(&o1)), content_hash(Some(&o2)));
    }

    #[test]
    fn content_hash_relation_rename_changes_hash() {
        let o1 = make_ontology(
            OntologyMode::Open,
            &[("Person", None)],
            &[("AUTHORED", None, None, None)],
        );
        let o2 = make_ontology(
            OntologyMode::Open,
            &[("Person", None)],
            &[("WROTE", None, None, None)],
        );
        assert_ne!(content_hash(Some(&o1)), content_hash(Some(&o2)));
    }

    #[test]
    fn content_hash_mode_flip_changes_hash() {
        let o1 = make_ontology(OntologyMode::Open, &[("Person", None)], &[]);
        let o2 = make_ontology(OntologyMode::Strict, &[("Person", None)], &[]);
        assert_ne!(content_hash(Some(&o1)), content_hash(Some(&o2)));
    }

    #[test]
    fn content_hash_description_update_changes_hash() {
        let o1 = make_ontology(OntologyMode::Open, &[("Person", None)], &[]);
        let o2 = make_ontology(
            OntologyMode::Open,
            &[("Person", Some("A human individual"))],
            &[],
        );
        assert_ne!(content_hash(Some(&o1)), content_hash(Some(&o2)));
    }

    #[test]
    fn content_hash_order_independent() {
        let o1 = make_ontology(
            OntologyMode::Open,
            &[("Person", None), ("Organization", None)],
            &[],
        );
        let o2 = make_ontology(
            OntologyMode::Open,
            &[("Organization", None), ("Person", None)],
            &[],
        );
        assert_eq!(content_hash(Some(&o1)), content_hash(Some(&o2)));
    }

    #[test]
    fn content_hash_none_differs_from_empty_would_be_same_sentinel() {
        // None always returns "none" — distinct from any real ontology hash
        let h = content_hash(None);
        assert_eq!(h, "none");
        assert_ne!(h.len(), 64); // real SHA-256 is 64 hex chars; "none" is 4
    }

    // ── parent / ancestor tests ───────────────────────────────────────────────

    #[test]
    fn load_ontology_2level_parent_builds_ancestor_map() {
        let dir = TempDir::new().unwrap();
        // Use pre-normalized names (PascalCase) to avoid surprises with name normalization
        write_ontology(
            &dir,
            "entity_types:\n  - name: Document\n  - name: Rfc\n    parent: Document\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load");
        assert_eq!(
            o.ancestor_map.get("Rfc"),
            Some(&vec!["Document".to_string()])
        );
        assert_eq!(o.ancestor_map.get("Document"), Some(&vec![]));
    }

    #[test]
    fn load_ontology_3level_chain_builds_full_ancestor_list() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: Document\n  - name: Rfc\n    parent: Document\n  - name: SubDoc\n    parent: Rfc\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load");
        // SubDoc → Rfc → Document: ancestors are [Document, Rfc] (supertype-first)
        assert_eq!(
            o.ancestor_map.get("SubDoc"),
            Some(&vec!["Document".to_string(), "Rfc".to_string()])
        );
        assert_eq!(
            o.ancestor_map.get("Rfc"),
            Some(&vec!["Document".to_string()])
        );
        assert_eq!(o.ancestor_map.get("Document"), Some(&vec![]));
    }

    #[test]
    fn load_ontology_flat_produces_empty_ancestor_vecs() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: Person\n  - name: Organization\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load");
        assert_eq!(o.ancestor_map.get("Person"), Some(&vec![]));
        assert_eq!(o.ancestor_map.get("Organization"), Some(&vec![]));
    }

    #[test]
    fn load_ontology_undeclared_parent_cleared_with_no_panic() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: Rfc\n    parent: NonExistent\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load without panicking");
        // Rfc should load with no parent (name stays as-is since it's already normalized)
        let rfc = o.entity_types.iter().find(|e| e.name == "Rfc");
        assert!(rfc.is_some(), "Rfc must still be present");
        assert!(rfc.unwrap().parent.is_none(), "undeclared parent must be cleared");
        assert_eq!(o.ancestor_map.get("Rfc"), Some(&vec![]));
    }

    #[test]
    fn load_ontology_2node_cycle_cleared_no_panic() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: A\n    parent: B\n  - name: B\n    parent: A\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load without panicking");
        let a = o.entity_types.iter().find(|e| e.name == "A").unwrap();
        let b = o.entity_types.iter().find(|e| e.name == "B").unwrap();
        assert!(a.parent.is_none(), "cycle member A must have parent cleared");
        assert!(b.parent.is_none(), "cycle member B must have parent cleared");
        assert_eq!(o.ancestor_map.get("A"), Some(&vec![]));
        assert_eq!(o.ancestor_map.get("B"), Some(&vec![]));
    }

    #[test]
    fn load_ontology_3node_cycle_all_cleared_no_panic() {
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: A\n    parent: C\n  - name: B\n    parent: A\n  - name: C\n    parent: B\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load without panicking");
        for name in ["A", "B", "C"] {
            let et = o.entity_types.iter().find(|e| e.name == name).unwrap();
            assert!(et.parent.is_none(), "cycle member {name} must have parent cleared");
            assert_eq!(o.ancestor_map.get(name), Some(&vec![]));
        }
    }

    #[test]
    fn load_ontology_entity_declared_explicitly_is_ignored() {
        // `Entity` is the implicit root — declaring it explicitly is silently dropped with a warning
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: Entity\n  - name: Person\n",
        );
        let o = load_ontology(Some(dir.path())).expect("should load without panicking");
        let names: Vec<_> = o.entity_types.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"Entity"), "explicit 'Entity' type must be filtered");
        assert!(names.contains(&"Person"), "other types must remain");
    }

    #[test]
    fn content_hash_flat_ontology_unchanged_vs_pre_173_format() {
        // Regression guard: flat ontologies (no parent fields) must produce the same hash
        // as pre-#173 code, because the `parent_edges:` segment is only appended when
        // at least one parent is declared (ADR-0053).
        // The canonical form for a flat ontology is identical to the pre-#173 form:
        //   "mode:{mode}\nentity_types:{entries}\nrelation_types:{entries}"
        let o = make_ontology(OntologyMode::Open, &[("Person", None)], &[]);
        // Manually build the expected canonical string using the pre-#173 format
        let expected_canonical =
            format!("mode:open\nentity_types:Person\0\nrelation_types:");
        let expected_hash = {
            let digest = sha2::Sha256::digest(expected_canonical.as_bytes());
            format!("{:x}", digest)
        };
        assert_eq!(
            content_hash(Some(&o)),
            expected_hash,
            "flat-ontology hash must match pre-#173 canonical form"
        );
    }

    #[test]
    fn content_hash_adding_parent_changes_hash() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "entity_types:\n  - name: Document\n  - name: RFC\n");
        let o_flat = load_ontology(Some(dir.path())).unwrap();

        write_ontology(
            &dir,
            "entity_types:\n  - name: Document\n  - name: RFC\n    parent: Document\n",
        );
        let o_hierarchy = load_ontology(Some(dir.path())).unwrap();

        let h_flat = content_hash(Some(&o_flat));
        let h_hierarchy = content_hash(Some(&o_hierarchy));
        assert_ne!(h_flat, h_hierarchy, "adding a parent must change the content hash");
        // Verify the hierarchy hash includes parent_edges in its canonical form
        assert_eq!(h_hierarchy.len(), 64, "hash must be a valid SHA-256 hex");
    }

    #[test]
    fn content_hash_parent_edges_order_independent() {
        // Two ontologies with the same parent edges in different declaration order produce the same hash
        let dir = TempDir::new().unwrap();
        write_ontology(
            &dir,
            "entity_types:\n  - name: A\n  - name: B\n    parent: A\n  - name: C\n    parent: A\n",
        );
        let o1 = load_ontology(Some(dir.path())).unwrap();

        write_ontology(
            &dir,
            "entity_types:\n  - name: A\n  - name: C\n    parent: A\n  - name: B\n    parent: A\n",
        );
        let o2 = load_ontology(Some(dir.path())).unwrap();

        assert_eq!(
            content_hash(Some(&o1)),
            content_hash(Some(&o2)),
            "parent_edges must be sorted — declaration order must not affect hash"
        );
    }

    #[test]
    fn content_hash_removing_parent_restores_flat_hash() {
        let dir = TempDir::new().unwrap();
        write_ontology(&dir, "entity_types:\n  - name: Document\n  - name: RFC\n");
        let o_flat = load_ontology(Some(dir.path())).unwrap();

        write_ontology(
            &dir,
            "entity_types:\n  - name: Document\n  - name: RFC\n    parent: Document\n",
        );
        let o_hierarchy = load_ontology(Some(dir.path())).unwrap();

        write_ontology(&dir, "entity_types:\n  - name: Document\n  - name: RFC\n");
        let o_restored = load_ontology(Some(dir.path())).unwrap();

        assert_ne!(content_hash(Some(&o_flat)), content_hash(Some(&o_hierarchy)));
        assert_eq!(
            content_hash(Some(&o_flat)),
            content_hash(Some(&o_restored)),
            "removing all parents must restore the flat-ontology hash"
        );
    }
}
