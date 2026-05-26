use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ontology::{content_hash, Ontology};

/// Persisted record of the ontology that was in effect during the last ingest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OntologySidecar {
    pub hash: String,
    pub mode: Option<String>,
    pub entity_types: Vec<String>,
    pub relation_types: Vec<String>,
}

pub fn sidecar_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".lcg").join("ontology-hash.json")
}

/// Reads the sidecar file. Returns `None` if the file is missing or unparseable.
pub fn read_sidecar(workspace_root: &Path) -> Option<OntologySidecar> {
    let path = sidecar_path(workspace_root);
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<OntologySidecar>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "liminis-graph: ontology-sidecar: failed to parse {:?}: {} — treating as absent",
                path, e
            );
            None
        }
    }
}

/// Atomically writes the sidecar file, recording the current ontology's hash and type lists.
pub fn write_sidecar(workspace_root: &Path, ontology: Option<&Ontology>) -> std::io::Result<()> {
    let lcg_dir = workspace_root.join(".lcg");
    std::fs::create_dir_all(&lcg_dir)?;

    let hash = content_hash(ontology);
    let (mode, entity_types, relation_types) = match ontology {
        Some(o) => (
            Some(o.mode.to_string()),
            o.entity_types.iter().map(|e| e.name.clone()).collect(),
            o.relation_types.iter().map(|r| r.name.clone()).collect(),
        ),
        None => (None, vec![], vec![]),
    };

    let sidecar = OntologySidecar {
        hash,
        mode,
        entity_types,
        relation_types,
    };

    let json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;

    let path = sidecar_path(workspace_root);
    let tmp_path = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Computes the drift state by comparing the current ontology's hash against the persisted sidecar.
/// Returns `(drifted, drift_summary)`.
pub fn compute_drift(
    workspace_root: Option<&Path>,
    ontology: Option<&Ontology>,
) -> (bool, Option<String>) {
    let root = match workspace_root {
        Some(r) => r,
        None => return (false, None),
    };

    let sidecar = match read_sidecar(root) {
        Some(s) => s,
        // No sidecar: first run or pre-upgrade workspace — no drift (FR-007)
        None => return (false, None),
    };

    let current_hash = content_hash(ontology);
    if current_hash == sidecar.hash {
        return (false, None);
    }

    let summary = build_drift_summary(&sidecar, ontology);
    (true, Some(summary))
}

fn build_drift_summary(sidecar: &OntologySidecar, current: Option<&Ontology>) -> String {
    let prev_entities: std::collections::HashSet<&str> =
        sidecar.entity_types.iter().map(|s| s.as_str()).collect();
    let prev_relations: std::collections::HashSet<&str> =
        sidecar.relation_types.iter().map(|s| s.as_str()).collect();

    let (cur_entities, cur_relations, mode_changed) = match current {
        Some(o) => {
            let ce: std::collections::HashSet<&str> =
                o.entity_types.iter().map(|e| e.name.as_str()).collect();
            let cr: std::collections::HashSet<&str> =
                o.relation_types.iter().map(|r| r.name.as_str()).collect();
            let mode_changed = sidecar.mode.as_deref() != Some(&o.mode.to_string());
            (ce, cr, mode_changed)
        }
        None => (
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
            sidecar.mode.is_some(),
        ),
    };

    let mut parts: Vec<String> = Vec::new();

    if mode_changed {
        let prev = sidecar.mode.as_deref().unwrap_or("none");
        let cur = current
            .map(|o| o.mode.to_string())
            .unwrap_or_else(|| "none".to_string());
        parts.push(format!("mode changed: {} → {}", prev, cur));
    }

    let added_entities: Vec<&str> = cur_entities
        .difference(&prev_entities)
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !added_entities.is_empty() {
        parts.push(format!(
            "entity types added: [{}]",
            added_entities.join(", ")
        ));
    }

    let removed_entities: Vec<&str> = prev_entities
        .difference(&cur_entities)
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !removed_entities.is_empty() {
        parts.push(format!(
            "entity types removed: [{}]",
            removed_entities.join(", ")
        ));
    }

    let added_relations: Vec<&str> = cur_relations
        .difference(&prev_relations)
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !added_relations.is_empty() {
        parts.push(format!(
            "relation types added: [{}]",
            added_relations.join(", ")
        ));
    }

    let removed_relations: Vec<&str> = prev_relations
        .difference(&cur_relations)
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !removed_relations.is_empty() {
        parts.push(format!(
            "relation types removed: [{}]",
            removed_relations.join(", ")
        ));
    }

    if parts.is_empty() {
        "descriptions or structure updated".to_string()
    } else {
        parts.join("; ")
    }
}
