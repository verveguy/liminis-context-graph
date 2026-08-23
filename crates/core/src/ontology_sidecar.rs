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
                "liminis-context-graph: ontology-sidecar: failed to parse {:?}: {} — treating as absent",
                path, e
            );
            None
        }
    }
}

/// Resolves the path a per-group drift sidecar would live at for `group_id` (issue #451,
/// FR-005): `{workspace_root}/.lcg/ontology-hash/<encoded_group_id>.json`. Mirrors
/// [`crate::ontology::group_ontology_path`]'s encoding and directory-per-group precedent exactly,
/// kept as a genuinely separate file (not a field folded into the existing single-valued
/// `.lcg/ontology-hash.json`) so that file's content/shape stays byte-identical for existing
/// single-ontology workspaces (FR-004).
pub fn group_sidecar_path(
    workspace_root: &Path,
    group_id: &str,
) -> Result<PathBuf, crate::error::Error> {
    let encoded = crate::wal_group::encode_group_dir_name(group_id)?;
    Ok(workspace_root
        .join(".lcg")
        .join("ontology-hash")
        .join(format!("{encoded}.json")))
}

/// Reads a group's drift sidecar. Returns `None` if `group_id` is invalid, the file is missing,
/// or it's unparseable — mirrors [`read_sidecar`]'s absent-on-any-failure behavior.
pub fn read_group_sidecar(workspace_root: &Path, group_id: &str) -> Option<OntologySidecar> {
    let path = group_sidecar_path(workspace_root, group_id).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<OntologySidecar>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "liminis-context-graph: ontology-sidecar: failed to parse group sidecar {:?} for group {:?}: {} — treating as absent",
                path, group_id, e
            );
            None
        }
    }
}

/// Atomically writes a group's drift sidecar, recording the resolved ontology's hash and type
/// lists for that group (issue #451). Mirrors [`write_sidecar`]'s shape and atomic-write pattern,
/// scoped to one group's file under `.lcg/ontology-hash/`.
pub fn write_group_sidecar(
    workspace_root: &Path,
    group_id: &str,
    ontology: Option<&Ontology>,
) -> std::io::Result<()> {
    let path = group_sidecar_path(workspace_root, group_id).map_err(std::io::Error::other)?;
    let dir = path
        .parent()
        .expect("group_sidecar_path always has a parent");
    std::fs::create_dir_all(dir)?;

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

    let tmp_path = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
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

/// File name (not full path — the record lives alongside a group's `.jsonl` files, same as
/// `.wal-generation.json` and `.wal-bounds.json`) of the published-ontology informational
/// sidecar (FR-007). Written into `<wal_root>/<group_id>/` right after an extraction guided by
/// that group's resolved ontology, so it travels automatically under the existing whole-directory
/// publish contract (see `docs/operations.md`).
pub const WAL_ONTOLOGY_FILE: &str = ".wal-ontology.json";

/// Resolves the path of the published-ontology sidecar within a group's WAL directory.
pub fn wal_ontology_path(wal_dir: &Path) -> PathBuf {
    wal_dir.join(WAL_ONTOLOGY_FILE)
}

/// Writes the published-ontology informational sidecar into a group's WAL directory (FR-007).
///
/// This file is **documentation only** (FR-008, FR-009): nothing in this codebase ever reads it
/// back, on either the producer or consumer side — there is no hydrate/replay code path that
/// consults it, so it can never drive a consumer's extraction, `mode: strict` validation,
/// canonicalization, or reprocessing. Its absence never blocks replay or affects correctness; it
/// only means a consumer inspecting the stream has no record of the vocabulary that produced it.
///
/// Reuses [`OntologySidecar`]'s existing shape rather than a second schema, since the two sidecars
/// (`.lcg/ontology-hash.json` for drift, `.wal-ontology.json` for publish provenance) record the
/// same information about an ontology, just at different scopes (workspace vs. per-group stream).
pub fn write_wal_ontology_sidecar(
    wal_dir: &Path,
    ontology: Option<&Ontology>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(wal_dir)?;

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

    let path = wal_ontology_path(wal_dir);
    // Unique per call: two concurrent same-group `add_episode` calls can reach this function
    // with no write lock held, so a shared temp name would let one writer's `File::create`
    // truncate a file the other hasn't finished writing, and one `rename` could publish the
    // truncated result.
    let tmp_path = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Reads the published-ontology informational sidecar from a group's WAL directory, if present.
/// Documentation-only per [`write_wal_ontology_sidecar`] — provided for external tooling/tests
/// that want to inspect it, not consulted by any lcg extraction/validation/reprocessing path.
pub fn read_wal_ontology_sidecar(wal_dir: &Path) -> Option<OntologySidecar> {
    let path = wal_ontology_path(wal_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<OntologySidecar>(&text).ok()
}

/// Computes the drift state by comparing the current ontology's hash against the persisted sidecar.
///
/// `has_prior_data`: true when no sidecar exists but the DB already contains ingested nodes
/// (pre-#98 workspace). In that case, loading an ontology is treated as drift (FR-002).
///
/// Returns `(drifted, drift_summary)`.
pub fn compute_drift(
    workspace_root: Option<&Path>,
    ontology: Option<&Ontology>,
    has_prior_data: bool,
) -> (bool, Option<String>) {
    let root = match workspace_root {
        Some(r) => r,
        None => return (false, None),
    };
    compute_drift_from_sidecar(read_sidecar(root), ontology, has_prior_data)
}

/// Per-group generalization of [`compute_drift`] (issue #451, FR-001/FR-005/FR-010): compares
/// the resolved ontology already handed to this group (whether via its own per-group file, the
/// workspace fallback, or neither) against `group_id`'s own persisted drift sidecar. Returns
/// `(false, None)` when `workspace_root` is `None` (no root to compare against — mirrors
/// `compute_drift`'s same fast path for direct-construction callers with no configured root).
pub fn compute_group_drift(
    workspace_root: Option<&Path>,
    group_id: &str,
    ontology: Option<&Ontology>,
    has_prior_data: bool,
) -> (bool, Option<String>) {
    let root = match workspace_root {
        Some(r) => r,
        None => return (false, None),
    };
    compute_drift_from_sidecar(read_group_sidecar(root, group_id), ontology, has_prior_data)
}

/// Shared drift comparison logic behind both [`compute_drift`] (workspace-scoped) and
/// [`compute_group_drift`] (per-group, issue #451): given whichever sidecar record was already
/// read for the relevant scope, decide whether `ontology`'s current hash has drifted from it.
///
/// `has_prior_data`: true when no sidecar exists but the DB already contains ingested nodes for
/// the relevant scope (a pre-#98 workspace, or — per FR-010 — a group whose data predates
/// per-group drift tracking). In that case, loading an ontology is treated as drift (FR-002).
///
/// Returns `(drifted, drift_summary)`.
fn compute_drift_from_sidecar(
    sidecar: Option<OntologySidecar>,
    ontology: Option<&Ontology>,
    has_prior_data: bool,
) -> (bool, Option<String>) {
    let sidecar = match sidecar {
        Some(s) => s,
        None => {
            // No prior record for this scope. If it already has data and an ontology is now
            // loaded, that's drift (FR-002, User Story 3 Scenario 2; FR-010 for the per-group case).
            if has_prior_data {
                if let Some(o) = ontology {
                    return (
                        true,
                        Some(format!(
                            "ontology added: {} entity types, {} relation types",
                            o.entity_types.len(),
                            o.relation_types.len()
                        )),
                    );
                }
            }
            return (false, None);
        }
    };

    let current_hash = content_hash(ontology);
    if current_hash == sidecar.hash {
        return (false, None);
    }

    let summary = build_drift_summary(&sidecar, ontology);
    (true, Some(summary))
}

fn build_drift_summary(sidecar: &OntologySidecar, current: Option<&Ontology>) -> String {
    // Pure addition: sidecar recorded "no ontology" but one is now loaded.
    if sidecar.hash == "none" {
        if let Some(o) = current {
            return format!(
                "ontology added: {} entity types, {} relation types",
                o.entity_types.len(),
                o.relation_types.len()
            );
        }
    }
    // Pure removal: sidecar recorded a real ontology but none is loaded now.
    if sidecar.hash != "none" && current.is_none() {
        return format!(
            "ontology removed (was {} entity types, {} relation types)",
            sidecar.entity_types.len(),
            sidecar.relation_types.len()
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::{EntityTypeDef, OntologyMode};
    use tempfile::TempDir;

    fn sample_ontology() -> Ontology {
        Ontology {
            mode: OntologyMode::Strict,
            entity_types: vec![EntityTypeDef {
                name: "KnowledgeChannel".to_string(),
                description: None,
                parent: None,
            }],
            relation_types: vec![],
            ancestor_map: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn wal_ontology_path_lives_alongside_wal_files() {
        let dir = TempDir::new().unwrap();
        let path = wal_ontology_path(dir.path());
        assert_eq!(path, dir.path().join(".wal-ontology.json"));
    }

    #[test]
    fn write_then_read_wal_ontology_sidecar_round_trips_some() {
        let dir = TempDir::new().unwrap();
        let ontology = sample_ontology();
        write_wal_ontology_sidecar(dir.path(), Some(&ontology)).unwrap();

        let sidecar = read_wal_ontology_sidecar(dir.path()).expect("sidecar should be present");
        assert_eq!(sidecar.mode.as_deref(), Some("strict"));
        assert_eq!(sidecar.entity_types, vec!["KnowledgeChannel".to_string()]);
        assert_eq!(sidecar.hash, content_hash(Some(&ontology)));
    }

    #[test]
    fn write_then_read_wal_ontology_sidecar_round_trips_none() {
        let dir = TempDir::new().unwrap();
        write_wal_ontology_sidecar(dir.path(), None).unwrap();

        let sidecar = read_wal_ontology_sidecar(dir.path()).expect("sidecar should be present");
        assert_eq!(sidecar.mode, None);
        assert!(sidecar.entity_types.is_empty());
        assert_eq!(sidecar.hash, content_hash(None));
    }

    #[test]
    fn read_wal_ontology_sidecar_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_wal_ontology_sidecar(dir.path()).is_none());
    }

    #[test]
    fn write_wal_ontology_sidecar_creates_wal_dir_if_missing() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("group-a");
        assert!(!wal_dir.exists());
        write_wal_ontology_sidecar(&wal_dir, None).unwrap();
        assert!(wal_ontology_path(&wal_dir).exists());
    }
}
