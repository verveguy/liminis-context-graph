use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    db::Conn,
    error::Error,
    types::{EntityRow, RelatesToEdge},
};

/// Batch size for `reprocess_entity_types` LLM classification.
/// Chosen to keep per-call prompt size manageable without excessive API round-trips.
pub(crate) const REPROCESS_BATCH_SIZE: usize = 50;

// ── YAML schema types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CorrectionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    /// For `same_as`: canonical entity name (used when `canonical_uuid` not given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    /// For `same_as`: canonical entity UUID (takes precedence over `canonical`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_uuid: Option<String>,
    /// For `same_as`: list of alias entity names to merge into the canonical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// For `retract`: UUID of the RelatesToNode_/RELATES_TO edge to invalidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_uuid: Option<String>,
    /// ISO-8601 UTC timestamp set by `apply_corrections`. Presence means already applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CorrectionsFile {
    pub corrections: Vec<CorrectionEntry>,
}

// ── Result types ──────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ValidateResult {
    pub valid: bool,
    pub message: String,
    pub total_corrections: usize,
    pub unapplied_corrections: usize,
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ApplyDetail {
    pub id: String,
    pub action: String,
}

#[derive(Debug, Default)]
pub struct ApplyResult {
    pub success: bool,
    pub applied: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
    pub details: Vec<ApplyDetail>,
    pub message: Option<String>,
}

#[derive(Debug, Default)]
pub struct ReprocessResult {
    pub success: bool,
    pub reclassified_count: usize,
    pub group_id: String,
    pub error: Option<String>,
}

// ── Merge-entities types ──────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct MergeEntitiesParams {
    pub canonical_uuid: Option<String>,
    pub canonical_name: Option<String>,
    pub alias_uuids: Vec<String>,
    pub alias_names: Vec<String>,
    pub merge_all_by_name: bool,
    pub group_id: String,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct AliasInfo {
    pub uuid: String,
    pub name: String,
    pub active_edges: usize,
    pub duplicate_edges: usize,
}

#[derive(Debug, Default)]
pub struct MergePlan {
    pub aliases: Vec<AliasInfo>,
    pub total_edges_rewritten: usize,
    pub total_edges_collapsed: usize,
}

#[derive(Debug, Default)]
pub struct MergeEntitiesResult {
    pub success: bool,
    pub canonical_uuid: String,
    pub merged_count: usize,
    pub skipped: usize,
    pub edges_rewritten: usize,
    pub edges_deduplicated: usize,
    pub errors: Vec<String>,
    pub plan: Option<MergePlan>,
}

// ── File location ─────────────────────────────────────────────────────────────

pub fn corrections_file_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".liminis")
        .join("knowledge-corrections.yaml")
}

// ── File I/O ──────────────────────────────────────────────────────────────────

/// Reads the corrections file. Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file cannot be read or parsed.
pub fn read_corrections_file(path: &Path) -> Result<Option<CorrectionsFile>, Error> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Ipc(format!("failed to read corrections file: {e}")))?;
    let file: CorrectionsFile =
        serde_yaml::from_str(&text).map_err(|e| Error::Ipc(format!("YAML parse error: {e}")))?;
    Ok(Some(file))
}

/// Atomically patches the `applied_at` field for the correction with the given `id`.
///
/// Preserves user-added YAML comments and formatting by operating on the raw text
/// rather than round-tripping through serde_yaml serialization.
/// Writes to `{path}.tmp` then renames to `path` atomically (POSIX rename semantics).
pub fn patch_applied_at(path: &Path, id: &str, ts: &str) -> Result<(), Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Ipc(format!("corrections file read error: {e}")))?;
    let lines: Vec<&str> = text.lines().collect();

    // Find the line containing this correction's id.
    // Handle both inline (`- id: <val>`) and standalone (`id: <val>`) forms,
    // with bare, double-quoted, or single-quoted values.
    let id_line_idx = lines
        .iter()
        .position(|l| {
            let t = l.trim();
            let val = t
                .strip_prefix("- id:")
                .or_else(|| t.strip_prefix("id:"))
                .map(|rest| rest.trim().trim_matches('"').trim_matches('\''));
            val == Some(id)
        })
        .ok_or_else(|| {
            Error::Ipc(format!(
                "correction id '{id}' not found in corrections file"
            ))
        })?;

    // Find the list item (`- `) that contains this id (scan backwards)
    let item_line_idx = (0..=id_line_idx)
        .rev()
        .find(|&i| lines[i].trim_start().starts_with("- "))
        .unwrap_or(id_line_idx);

    let item_indent = lines[item_line_idx].len() - lines[item_line_idx].trim_start().len();

    // Find end of this correction's block (next `- ` at same indent or end of file)
    let block_end = (item_line_idx + 1..lines.len())
        .find(|&i| {
            let line = lines[i];
            let indent = line.len() - line.trim_start().len();
            !line.trim().is_empty() && indent == item_indent && line.trim_start().starts_with("- ")
        })
        .unwrap_or(lines.len());

    // Key indentation is item_indent + 2 (standard 2-space YAML indent)
    let field_indent = " ".repeat(item_indent + 2);
    let new_line = format!("{field_indent}applied_at: \"{ts}\"");

    // Check if applied_at already exists in the block. Scan from item_line_idx
    // rather than id_line_idx because YAML keys are unordered — applied_at may appear
    // before id in the serialized output.
    let applied_at_idx =
        (item_line_idx..block_end).find(|&i| lines[i].trim_start().starts_with("applied_at:"));

    let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    if let Some(idx) = applied_at_idx {
        new_lines[idx] = new_line;
    } else {
        new_lines.insert(id_line_idx + 1, new_line);
    }

    let new_text = new_lines.join("\n");
    let new_text = if text.ends_with('\n') {
        new_text + "\n"
    } else {
        new_text
    };

    let tmp_path = path.with_extension("yaml.tmp");
    std::fs::write(&tmp_path, &new_text)
        .map_err(|e| Error::Ipc(format!("corrections tmp write error: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Error::Ipc(format!("corrections file rename error: {e}")))?;
    Ok(())
}

// ── Cycle detection ───────────────────────────────────────────────────────────

/// Detects alias cycles in `same_as` corrections (e.g. A→B→C→A).
/// Returns a description string for each cycle found.
pub fn detect_cycles(entries: &[CorrectionEntry]) -> Vec<String> {
    // Build a map: alias_name → canonical_name (or uuid if only uuid given)
    let mut alias_to_canonical: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if entry.type_.to_lowercase() == "same_as" {
            let canonical = entry
                .canonical
                .as_deref()
                .or(entry.canonical_uuid.as_deref())
                .unwrap_or_default()
                .to_string();
            if let Some(ref aliases) = entry.aliases {
                for alias in aliases {
                    alias_to_canonical.insert(alias.clone(), canonical.clone());
                }
            }
        }
    }

    let mut cycles = Vec::new();
    // 0 = unvisited, 1 = in current path, 2 = done
    let mut state: HashMap<String, u8> = HashMap::new();
    let nodes: Vec<String> = alias_to_canonical.keys().cloned().collect();

    for start in &nodes {
        if *state.get(start).unwrap_or(&0) == 0 {
            let mut path: Vec<String> = Vec::new();
            dfs_detect(
                start,
                &alias_to_canonical,
                &mut state,
                &mut path,
                &mut cycles,
            );
        }
    }

    cycles
}

fn dfs_detect(
    node: &str,
    graph: &HashMap<String, String>,
    state: &mut HashMap<String, u8>,
    path: &mut Vec<String>,
    cycles: &mut Vec<String>,
) {
    state.insert(node.to_string(), 1);
    path.push(node.to_string());

    if let Some(next) = graph.get(node) {
        match *state.get(next.as_str()).unwrap_or(&0) {
            1 => {
                // Back edge → cycle found
                let cycle_start = path.iter().position(|n| n == next).unwrap_or(0);
                let cycle_nodes: Vec<&str> =
                    path[cycle_start..].iter().map(|s| s.as_str()).collect();
                cycles.push(format!(
                    "Alias cycle: {} -> {}",
                    cycle_nodes.join(" -> "),
                    next
                ));
            }
            0 => dfs_detect(next, graph, state, path, cycles),
            _ => {}
        }
    }

    path.pop();
    state.insert(node.to_string(), 2);
}

// ── validate_corrections_file ─────────────────────────────────────────────────

pub fn validate_corrections_file(conn: &Conn, workspace_root: &Path) -> ValidateResult {
    let path = corrections_file_path(workspace_root);

    let file = match read_corrections_file(&path) {
        Ok(None) => {
            return ValidateResult {
                valid: true,
                message: "No corrections file found".to_string(),
                total_corrections: 0,
                unapplied_corrections: 0,
                issues: vec![],
                warnings: vec![],
            };
        }
        Ok(Some(f)) => f,
        Err(e) => {
            return ValidateResult {
                valid: false,
                message: "Failed to read corrections file".to_string(),
                total_corrections: 0,
                unapplied_corrections: 0,
                issues: vec![e.to_string()],
                warnings: vec![],
            };
        }
    };

    let total = file.corrections.len();
    let unapplied = file
        .corrections
        .iter()
        .filter(|e| e.applied_at.is_none())
        .count();
    let mut issues: Vec<String> = Vec::new();
    let warnings: Vec<String> = Vec::new();

    for entry in &file.corrections {
        match entry.type_.to_lowercase().as_str() {
            "same_as" => {
                if entry.canonical.is_none() && entry.canonical_uuid.is_none() {
                    issues.push(format!(
                        "Correction '{}': same_as requires 'canonical' or 'canonical_uuid'",
                        entry.id
                    ));
                }
                if entry.aliases.as_ref().map(|a| a.is_empty()).unwrap_or(true) {
                    issues.push(format!(
                        "Correction '{}': same_as requires non-empty 'aliases'",
                        entry.id
                    ));
                }
                if let Some(ref uuid) = entry.canonical_uuid {
                    match conn.get_entity_by_uuid(uuid) {
                        Ok(None) => issues.push(format!(
                            "Correction '{}': canonical_uuid '{}' not found in graph",
                            entry.id, uuid
                        )),
                        Err(e) => issues.push(format!(
                            "Correction '{}': canonical_uuid graph lookup error: {e}",
                            entry.id
                        )),
                        Ok(Some(_)) => {}
                    }
                }
            }
            "retract" => {
                if let Some(ref uuid) = entry.edge_uuid {
                    match conn.get_edge_by_uuid(uuid) {
                        Ok(None) => issues.push(format!(
                            "Correction '{}': edge_uuid '{}' not found in graph",
                            entry.id, uuid
                        )),
                        Err(e) => issues.push(format!(
                            "Correction '{}': edge_uuid graph lookup error: {e}",
                            entry.id
                        )),
                        Ok(Some(_)) => {}
                    }
                } else {
                    issues.push(format!(
                        "Correction '{}': retract requires 'edge_uuid'",
                        entry.id
                    ));
                }
            }
            other => {
                issues.push(format!("Correction '{}': unknown type '{other}'", entry.id));
            }
        }
    }

    let cycle_issues = detect_cycles(&file.corrections);
    issues.extend(cycle_issues);

    let valid = issues.is_empty();
    ValidateResult {
        valid,
        message: if valid {
            "All corrections are valid".to_string()
        } else {
            format!("{} issue(s) found", issues.len())
        },
        total_corrections: total,
        unapplied_corrections: unapplied,
        issues,
        warnings,
    }
}

// ── apply_corrections_file ────────────────────────────────────────────────────

pub fn apply_corrections_file(conn: &Conn, workspace_root: &Path, dry_run: bool) -> ApplyResult {
    let path = corrections_file_path(workspace_root);

    let file = match read_corrections_file(&path) {
        Ok(None) => {
            return ApplyResult {
                success: true,
                message: Some("No corrections file found".to_string()),
                ..Default::default()
            };
        }
        Ok(Some(f)) if f.corrections.is_empty() => {
            return ApplyResult {
                success: true,
                message: Some("Corrections file is empty".to_string()),
                ..Default::default()
            };
        }
        Ok(Some(f)) => f,
        Err(e) => {
            return ApplyResult {
                success: false,
                errors: vec![format!("Failed to read corrections file: {e}")],
                ..Default::default()
            };
        }
    };

    let now_ts = Utc::now().to_rfc3339();
    let mut applied = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut details: Vec<ApplyDetail> = Vec::new();

    for entry in &file.corrections {
        if entry.applied_at.is_some() {
            skipped += 1;
            continue;
        }

        let result = match entry.type_.to_lowercase().as_str() {
            "same_as" => apply_same_as(conn, entry, &path, &now_ts, dry_run),
            "retract" => apply_retract(conn, entry, &path, &now_ts, dry_run),
            other => Err(Error::Ipc(format!(
                "Correction '{}': unknown type '{other}'",
                entry.id
            ))),
        };

        match result {
            Ok(action) => {
                // In dry_run mode, nothing is actually applied (FR-015: "applied: 0").
                if !dry_run {
                    applied += 1;
                }
                details.push(ApplyDetail {
                    id: entry.id.clone(),
                    action,
                });
            }
            Err(e) => {
                errors.push(format!("Correction '{}': {e}", entry.id));
            }
        }
    }

    ApplyResult {
        success: errors.is_empty(),
        applied,
        skipped,
        errors,
        details,
        message: None,
    }
}

fn apply_same_as(
    conn: &Conn,
    entry: &CorrectionEntry,
    path: &Path,
    ts: &str,
    dry_run: bool,
) -> Result<String, Error> {
    // Resolve canonical entity UUID
    let canonical_entity = resolve_canonical(conn, entry)?;
    let canonical_uuid = canonical_entity.uuid.clone();
    let canonical_group = canonical_entity.group_id.clone();

    let aliases = entry.aliases.as_deref().unwrap_or(&[]);
    if aliases.is_empty() {
        return Err(Error::Ipc(
            "same_as requires non-empty 'aliases'".to_string(),
        ));
    }

    for alias_name in aliases {
        // Find alias entity by name in same group as canonical
        let alias_entity = conn
            .get_entity_by_name(alias_name, &canonical_group)?
            .ok_or_else(|| {
                Error::Ipc(format!(
                    "alias entity '{alias_name}' not found in group '{canonical_group}'"
                ))
            })?;
        let alias_uuid = alias_entity.uuid.clone();

        if alias_uuid == canonical_uuid {
            continue; // skip self-merge
        }

        if !dry_run {
            // Move edges from alias to canonical
            let alias_edges = conn.get_full_edges_for_entity(&alias_uuid)?;
            for old_edge in &alias_edges {
                if old_edge.invalid_at.is_some() {
                    continue; // already invalid, skip
                }
                // Compute new source/target (replace alias with canonical)
                let new_src = if old_edge.source_node_uuid == alias_uuid {
                    canonical_uuid.clone()
                } else {
                    old_edge.source_node_uuid.clone()
                };
                let new_dst = if old_edge.target_node_uuid == alias_uuid {
                    canonical_uuid.clone()
                } else {
                    old_edge.target_node_uuid.clone()
                };

                // De-duplicate: skip if canonical already has an edge with the same name to the same endpoint
                if conn.has_directed_edge(&new_src, &new_dst, &old_edge.name)? {
                    conn.invalidate_edge(&old_edge.uuid, ts)?;
                    continue;
                }

                // Create replacement edge on canonical
                let new_edge = RelatesToEdge {
                    uuid: Uuid::new_v4().to_string(),
                    name: old_edge.name.clone(),
                    source_node_uuid: new_src,
                    target_node_uuid: new_dst,
                    group_id: old_edge.group_id.clone(),
                    fact: old_edge.fact.clone(),
                    fact_embedding: old_edge.fact_embedding.clone(),
                    created_at: old_edge.created_at.clone(),
                    valid_at: old_edge.valid_at.clone(),
                    invalid_at: None,
                    attributes: old_edge.attributes.clone(),
                    relation_type: old_edge.relation_type.clone(),
                    episode_uuids: vec![],
                    source_descriptions: vec![],
                };
                conn.insert_relates_to_edge(&new_edge)?;

                // Invalidate old alias edge
                conn.invalidate_edge(&old_edge.uuid, ts)?;
            }

            // Mark alias entity as merged
            let mut new_labels = alias_entity.labels.clone();
            if !new_labels.contains(&"Merged".to_string()) {
                new_labels.push("Merged".to_string());
            }
            conn.update_entity_labels(&alias_uuid, &new_labels)?;
        }
    }

    if !dry_run {
        patch_applied_at(path, &entry.id, ts)?;
    }

    Ok(if dry_run {
        "dry_run:same_as"
    } else {
        "same_as"
    }
    .to_string())
}

fn apply_retract(
    conn: &Conn,
    entry: &CorrectionEntry,
    path: &Path,
    ts: &str,
    dry_run: bool,
) -> Result<String, Error> {
    let edge_uuid = entry
        .edge_uuid
        .as_deref()
        .ok_or_else(|| Error::Ipc("retract requires 'edge_uuid'".to_string()))?;

    // Verify edge exists (same check as validate_corrections per FR-015 dry_run requirement)
    conn.get_edge_by_uuid(edge_uuid)?
        .ok_or_else(|| Error::Ipc(format!("edge_uuid '{edge_uuid}' not found in graph")))?;

    if !dry_run {
        conn.invalidate_edge(edge_uuid, ts)?;
        patch_applied_at(path, &entry.id, ts)?;
    }

    Ok(if dry_run {
        "dry_run:retract"
    } else {
        "retract"
    }
    .to_string())
}

fn resolve_canonical(conn: &Conn, entry: &CorrectionEntry) -> Result<EntityRow, Error> {
    // Prefer canonical_uuid
    if let Some(ref uuid) = entry.canonical_uuid {
        return conn
            .get_entity_by_uuid(uuid)?
            .ok_or_else(|| Error::Ipc(format!("canonical_uuid '{uuid}' not found in graph")));
    }

    // Fall back to canonical name (search across all groups — no group_id in corrections entry)
    if let Some(ref name) = entry.canonical {
        // Try the first group found for this entity name using search_entities
        let candidates = conn.search_entities(name)?;
        return candidates
            .into_iter()
            .find(|e| e.name == *name)
            .ok_or_else(|| Error::Ipc(format!("canonical entity '{name}' not found in graph")));
    }

    Err(Error::Ipc(
        "same_as requires 'canonical' or 'canonical_uuid'".to_string(),
    ))
}

// ── reprocess helpers (used by handlers to implement phase-split) ─────────────

/// Phase A: lists all generic-only entities for a group, paged.
/// Returns all pages concatenated.
pub fn list_all_generic_entities(conn: &Conn, group_id: &str) -> Result<Vec<EntityRow>, Error> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let page = conn.list_generic_entities_page(group_id, offset, REPROCESS_BATCH_SIZE)?;
        let page_len = page.len();
        all.extend(page);
        if page_len < REPROCESS_BATCH_SIZE {
            break;
        }
        offset += REPROCESS_BATCH_SIZE;
    }
    Ok(all)
}

/// Phase B: applies specific entity type labels.
/// `updates` is a slice of (entity_uuid, specific_type_label) pairs.
/// `ancestor_map` is precomputed from the workspace ontology and used to stamp ancestor labels.
/// Returns the number of entities actually updated.
pub fn apply_entity_type_labels(
    conn: &Conn,
    updates: &[(String, String)],
    ancestor_map: &std::collections::HashMap<String, Vec<String>>,
) -> Result<usize, Error> {
    let mut count = 0;
    for (uuid, entity_type) in updates {
        if entity_type.is_empty() {
            continue;
        }
        let mut labels = vec!["Entity".to_string()];
        if let Some(ancestors) = ancestor_map.get(entity_type) {
            labels.extend(ancestors.iter().cloned());
        }
        labels.push(entity_type.clone());
        conn.update_entity_labels(uuid, &labels)?;
        count += 1;
    }
    Ok(count)
}

/// Phase D helper: lists all typed entities (those with a specific type label beyond "Entity").
/// Returns all pages concatenated.
pub fn list_all_typed_entities(conn: &Conn, group_id: &str) -> Result<Vec<EntityRow>, Error> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let page = conn.list_typed_entities_page(group_id, offset, REPROCESS_BATCH_SIZE)?;
        let page_len = page.len();
        all.extend(page);
        if page_len < REPROCESS_BATCH_SIZE {
            break;
        }
        offset += REPROCESS_BATCH_SIZE;
    }
    Ok(all)
}

// ── reprocess_entity_types scope helpers ─────────────────────────────────────

/// Scope of entities targeted by `reprocess_entity_types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprocessScope {
    /// Only entities with no specific type (sole label is `Entity`). Default; preserves
    /// pre-#177 behavior.
    Untyped,
    /// Entities whose specific type is absent from the declared ontology, plus untyped
    /// entities. Requires an ontology to be loaded.
    OffOntology,
    /// Every entity in the group, regardless of current type. Requires an ontology.
    All,
}

/// Finds the leaf (most-derived) specific entity type from a label set.
///
/// Uses `ancestor_map` to identify a declared type that is not an ancestor of any other
/// specific label on the same entity. Returns `None` if no unambiguous leaf is found (e.g.,
/// the type is not declared in `ancestor_map`, or there are multiple independent leaf types).
pub(crate) fn find_leaf_type(
    labels: &[String],
    ancestor_map: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let specific: Vec<&str> = labels
        .iter()
        .filter(|l| l.as_str() != "Entity")
        .map(|l| l.as_str())
        .collect();
    let leaf_types: Vec<&str> = specific
        .iter()
        .copied()
        .filter(|&t| {
            ancestor_map.contains_key(t)
                && !specific.iter().any(|&other| {
                    t != other
                        && ancestor_map
                            .get(other)
                            .is_some_and(|anc| anc.iter().any(|a| a.as_str() == t))
                })
        })
        .collect();
    if leaf_types.len() == 1 {
        Some(leaf_types[0].to_string())
    } else {
        None
    }
}

/// Returns `true` if an entity's specific type is absent from the declared ontology.
///
/// For hierarchical ontologies (`ancestor_map` non-empty), uses `find_leaf_type` to determine
/// the most-derived specific type, then checks it against `type_names`.
/// For flat ontologies (types not declared in `ancestor_map`), falls back to checking whether
/// any non-`Entity` label is absent from `type_names`.
fn is_off_ontology(
    labels: &[String],
    type_names: &std::collections::HashSet<String>,
    ancestor_map: &HashMap<String, Vec<String>>,
) -> bool {
    if let Some(leaf) = find_leaf_type(labels, ancestor_map) {
        return !type_names.contains(&leaf);
    }
    // Flat ontology or undeclared types: off-ontology if any non-Entity label is absent.
    labels
        .iter()
        .filter(|l| l.as_str() != "Entity")
        .any(|l| !type_names.contains(l))
}

/// Collects candidate entities for `reprocess_entity_types` based on `scope`.
///
/// - `Untyped`: entities with no specific type (preserves pre-#177 behavior).
/// - `OffOntology`: untyped entities plus typed entities whose leaf type is absent from
///   `ontology_type_names`. `ontology_type_names` must be `Some`.
/// - `All`: every entity in the group. `ontology_type_names` must be `Some`.
pub(crate) fn list_entities_for_scope(
    conn: &Conn,
    group_id: &str,
    scope: ReprocessScope,
    ontology_type_names: Option<&std::collections::HashSet<String>>,
    ancestor_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<EntityRow>, Error> {
    match scope {
        ReprocessScope::Untyped => list_all_generic_entities(conn, group_id),
        ReprocessScope::OffOntology => {
            let type_names =
                ontology_type_names.expect("caller must supply type_names for OffOntology scope");
            let mut candidates = list_all_generic_entities(conn, group_id)?;
            for entity in list_all_typed_entities(conn, group_id)? {
                if is_off_ontology(&entity.labels, type_names, ancestor_map) {
                    candidates.push(entity);
                }
            }
            Ok(candidates)
        }
        ReprocessScope::All => {
            let mut all = list_all_generic_entities(conn, group_id)?;
            all.extend(list_all_typed_entities(conn, group_id)?);
            Ok(all)
        }
    }
}

// ── merge_entities ────────────────────────────────────────────────────────────

/// Inner edge-rewrite loop for a single alias → canonical merge.
///
/// Returns `(edges_rewritten, edges_deduplicated, self_loops_dropped)`.
/// In `dry_run` mode, reads DB state to compute counts but does not mutate anything.
fn merge_entities_inner(
    conn: &Conn,
    canonical_uuid: &str,
    alias: &EntityRow,
    ts: &str,
    dry_run: bool,
) -> Result<(usize, usize, usize), Error> {
    let alias_uuid = &alias.uuid;
    let alias_edges = conn.get_full_edges_for_entity(alias_uuid)?;

    let mut rewritten = 0usize;
    let mut deduped = 0usize;
    let mut self_loops = 0usize;

    for old_edge in &alias_edges {
        if old_edge.invalid_at.is_some() {
            continue; // skip already-invalid edges (FR-011)
        }

        let new_src = if old_edge.source_node_uuid == *alias_uuid {
            canonical_uuid.to_string()
        } else {
            old_edge.source_node_uuid.clone()
        };
        let new_dst = if old_edge.target_node_uuid == *alias_uuid {
            canonical_uuid.to_string()
        } else {
            old_edge.target_node_uuid.clone()
        };

        // Drop self-loop edges that would arise from merging two ends of an existing edge (FR-010)
        if new_src == new_dst {
            if !dry_run {
                conn.invalidate_edge(&old_edge.uuid, ts)?;
            }
            self_loops += 1;
            continue;
        }

        // Dedup: skip if canonical already has this directed edge (FR-009)
        if conn.has_directed_edge(&new_src, &new_dst, &old_edge.name)? {
            if !dry_run {
                conn.invalidate_edge(&old_edge.uuid, ts)?;
            }
            deduped += 1;
            continue;
        }

        if !dry_run {
            let new_edge = RelatesToEdge {
                uuid: Uuid::new_v4().to_string(),
                name: old_edge.name.clone(),
                source_node_uuid: new_src,
                target_node_uuid: new_dst,
                group_id: old_edge.group_id.clone(),
                fact: old_edge.fact.clone(),
                fact_embedding: old_edge.fact_embedding.clone(),
                created_at: old_edge.created_at.clone(),
                valid_at: old_edge.valid_at.clone(),
                invalid_at: None,
                attributes: old_edge.attributes.clone(),
                relation_type: old_edge.relation_type.clone(),
                episode_uuids: vec![],
                source_descriptions: vec![],
            };
            conn.insert_relates_to_edge(&new_edge)?;
            conn.invalidate_edge(&old_edge.uuid, ts)?;
        }
        rewritten += 1;
    }

    Ok((rewritten, deduped, self_loops))
}

/// Merges one or more alias entities into a canonical entity.
///
/// Resolves the canonical by UUID (preferred) or by name (earliest `created_at, uuid`).
/// Alias sets are resolved from explicit UUIDs, explicit names, and/or `merge_all_by_name`.
/// In dry-run mode, returns the merge plan without mutating the graph.
pub fn merge_entities(conn: &Conn, params: &MergeEntitiesParams, ts: &str) -> MergeEntitiesResult {
    // Validate inputs
    if params.canonical_uuid.is_none() && params.canonical_name.is_none() {
        return MergeEntitiesResult {
            success: false,
            errors: vec![
                "at least one of canonical_uuid or canonical_name must be provided".to_string(),
            ],
            ..Default::default()
        };
    }
    if params.alias_uuids.is_empty() && params.alias_names.is_empty() && !params.merge_all_by_name {
        return MergeEntitiesResult {
            success: false,
            errors: vec![
                "at least one of alias_uuids, alias_names, or merge_all_by_name must be provided"
                    .to_string(),
            ],
            ..Default::default()
        };
    }

    let group_id = if params.group_id.is_empty() {
        "liminis"
    } else {
        &params.group_id
    };

    // Resolve canonical entity (FR-003)
    let canonical = match &params.canonical_uuid {
        Some(uuid) => match conn.get_entity_by_uuid(uuid) {
            Ok(Some(e)) => e,
            Ok(None) => {
                return MergeEntitiesResult {
                    success: false,
                    errors: vec![format!("canonical entity not found: uuid={uuid}")],
                    ..Default::default()
                };
            }
            Err(e) => {
                return MergeEntitiesResult {
                    success: false,
                    errors: vec![format!("canonical lookup failed: {e}")],
                    ..Default::default()
                };
            }
        },
        None => {
            let name = params.canonical_name.as_deref().unwrap();
            match conn.get_entities_by_name_all(name, group_id) {
                Ok(mut rows) => {
                    // Skip already-merged entities — canonical must be active (FR-003).
                    // A previous UUID-based merge may have left the chronologically-earliest
                    // entity marked as merged; we want the earliest *active* entity.
                    rows.retain(|r| !r.labels.contains(&"Merged".to_string()));
                    if rows.is_empty() {
                        return MergeEntitiesResult {
                            success: false,
                            errors: vec![format!(
                                "canonical entity not found: name={name}, group={group_id}"
                            )],
                            ..Default::default()
                        };
                    }
                    rows.remove(0)
                }
                Err(e) => {
                    return MergeEntitiesResult {
                        success: false,
                        errors: vec![format!("canonical lookup failed: {e}")],
                        ..Default::default()
                    };
                }
            }
        }
    };

    // Canonical must not already be merged
    if canonical.labels.contains(&"Merged".to_string()) {
        return MergeEntitiesResult {
            success: false,
            canonical_uuid: canonical.uuid.clone(),
            errors: vec![
                "canonical entity is already merged — cannot use as merge target".to_string(),
            ],
            ..Default::default()
        };
    }

    let canonical_uuid = canonical.uuid.clone();

    // Expand alias set (FR-004, FR-005)
    let mut alias_map: std::collections::HashMap<String, EntityRow> =
        std::collections::HashMap::new();
    let mut errors: Vec<String> = Vec::new();

    // From explicit UUIDs
    if !params.alias_uuids.is_empty() {
        match conn.get_entities_by_uuids(&params.alias_uuids) {
            Ok(rows) => {
                for row in rows {
                    if row.uuid != canonical_uuid {
                        alias_map.insert(row.uuid.clone(), row);
                    }
                }
                // Report UUIDs that weren't found
                for uuid in &params.alias_uuids {
                    if uuid != &canonical_uuid && !alias_map.contains_key(uuid) {
                        errors.push(format!("alias uuid not found: {uuid}"));
                    }
                }
            }
            Err(e) => errors.push(format!("alias UUID lookup failed: {e}")),
        }
    }

    // From explicit names
    for alias_name in &params.alias_names {
        match conn.get_entities_by_name_all(alias_name, group_id) {
            Ok(rows) => {
                if rows.is_empty() {
                    errors.push(format!(
                        "alias name '{alias_name}' matches no entities in group '{group_id}'"
                    ));
                }
                for row in rows {
                    if row.uuid != canonical_uuid {
                        alias_map.entry(row.uuid.clone()).or_insert(row);
                    }
                }
            }
            Err(e) => errors.push(format!("alias name lookup failed for '{alias_name}': {e}")),
        }
    }

    // From merge_all_by_name: all same-name entities in same group except the canonical
    if params.merge_all_by_name {
        let name = canonical.name.as_str();
        match conn.get_entities_by_name_all(name, group_id) {
            Ok(rows) => {
                for row in rows {
                    if row.uuid != canonical_uuid {
                        alias_map.entry(row.uuid.clone()).or_insert(row);
                    }
                }
            }
            Err(e) => errors.push(format!("merge_all_by_name lookup failed: {e}")),
        }
    }

    // Process aliases
    let mut merged_count = 0usize;
    let mut skipped = 0usize;
    let mut total_rewritten = 0usize;
    let mut total_deduped = 0usize;
    let mut plan_aliases: Vec<AliasInfo> = Vec::new();
    let mut earliest_created_at = canonical.created_at.clone();

    // Collect and sort aliases for deterministic ordering
    let mut aliases: Vec<EntityRow> = alias_map.into_values().collect();
    aliases.sort_by(|a, b| a.uuid.cmp(&b.uuid));

    for alias in &aliases {
        // Skip already-merged aliases (FR-013)
        if alias.labels.contains(&"Merged".to_string()) {
            skipped += 1;
            continue;
        }

        match merge_entities_inner(conn, &canonical_uuid, alias, ts, params.dry_run) {
            Ok((rewritten, deduped, _self_loops)) => {
                total_rewritten += rewritten;
                total_deduped += deduped;

                if params.dry_run {
                    plan_aliases.push(AliasInfo {
                        uuid: alias.uuid.clone(),
                        name: alias.name.clone(),
                        active_edges: rewritten,
                        duplicate_edges: deduped,
                    });
                } else {
                    // Mark alias as merged (FR-008)
                    let mut new_labels = alias.labels.clone();
                    if !new_labels.contains(&"Merged".to_string()) {
                        new_labels.push("Merged".to_string());
                    }
                    if let Err(e) = conn.update_entity_labels(&alias.uuid, &new_labels) {
                        errors.push(format!(
                            "failed to mark alias {} as merged: {e}",
                            alias.uuid
                        ));
                    }
                    // Track earliest created_at across canonical + aliases (FR-007)
                    if !alias.created_at.is_empty() && alias.created_at < earliest_created_at {
                        earliest_created_at = alias.created_at.clone();
                    }
                }
                merged_count += 1;
            }
            Err(e) => {
                errors.push(format!("merge failed for alias {}: {e}", alias.uuid));
            }
        }
    }

    // Update canonical's created_at to earliest value across all merged (FR-007)
    if !params.dry_run && merged_count > 0 && earliest_created_at != canonical.created_at {
        if let Err(e) = conn.update_entity_created_at(&canonical_uuid, &earliest_created_at) {
            errors.push(format!("failed to update canonical created_at: {e}"));
        }
    }

    let plan = if params.dry_run {
        Some(MergePlan {
            total_edges_rewritten: total_rewritten,
            total_edges_collapsed: total_deduped,
            aliases: plan_aliases,
        })
    } else {
        None
    };

    MergeEntitiesResult {
        success: errors.iter().all(|e| {
            // "alias … not found" and "matches no entities" are soft warnings — processing
            // continues and the caller already saw them in errors[]. All other errors
            // (DB write failures, lookup errors) are hard failures that set success=false.
            e.contains("not found") || e.contains("matches no entities")
        }),
        canonical_uuid,
        merged_count,
        skipped,
        edges_rewritten: total_rewritten,
        edges_deduplicated: total_deduped,
        errors,
        plan,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[allow(dead_code)]
    fn write_corrections(dir: &TempDir, content: &str) -> PathBuf {
        let liminis_dir = dir.path().join(".liminis");
        std::fs::create_dir_all(&liminis_dir).unwrap();
        let path = liminis_dir.join("knowledge-corrections.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_read_corrections_file_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.yaml");
        let result = read_corrections_file(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_corrections_file_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "not: valid: yaml: content: [{{").unwrap();
        let result = read_corrections_file(&path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("YAML parse error"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn test_read_corrections_file_empty_list() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.yaml");
        std::fs::write(&path, "corrections: []\n").unwrap();
        let result = read_corrections_file(&path).unwrap().unwrap();
        assert_eq!(result.corrections.len(), 0);
    }

    #[test]
    fn test_detect_cycles_no_cycle() {
        let entries = vec![
            CorrectionEntry {
                id: "c1".to_string(),
                type_: "same_as".to_string(),
                canonical: Some("Alice".to_string()),
                canonical_uuid: None,
                aliases: Some(vec!["Al".to_string()]),
                edge_uuid: None,
                applied_at: None,
            },
            CorrectionEntry {
                id: "c2".to_string(),
                type_: "same_as".to_string(),
                canonical: Some("Bob".to_string()),
                canonical_uuid: None,
                aliases: Some(vec!["B".to_string()]),
                edge_uuid: None,
                applied_at: None,
            },
        ];
        assert!(detect_cycles(&entries).is_empty());
    }

    #[test]
    fn test_detect_cycles_with_cycle() {
        // A→B, B→C, C→A
        let entries = vec![
            CorrectionEntry {
                id: "c1".to_string(),
                type_: "same_as".to_string(),
                canonical: Some("B".to_string()),
                canonical_uuid: None,
                aliases: Some(vec!["A".to_string()]),
                edge_uuid: None,
                applied_at: None,
            },
            CorrectionEntry {
                id: "c2".to_string(),
                type_: "same_as".to_string(),
                canonical: Some("C".to_string()),
                canonical_uuid: None,
                aliases: Some(vec!["B".to_string()]),
                edge_uuid: None,
                applied_at: None,
            },
            CorrectionEntry {
                id: "c3".to_string(),
                type_: "same_as".to_string(),
                canonical: Some("A".to_string()),
                canonical_uuid: None,
                aliases: Some(vec!["C".to_string()]),
                edge_uuid: None,
                applied_at: None,
            },
        ];
        let cycles = detect_cycles(&entries);
        assert!(!cycles.is_empty(), "expected a cycle to be detected");
    }

    #[test]
    fn test_patch_applied_at_inserts_field() {
        let dir = TempDir::new().unwrap();
        let content = "corrections:\n  - id: corr-001\n    type: retract\n    edge_uuid: abc\n";
        let path = dir.path().join("corrections.yaml");
        std::fs::write(&path, content).unwrap();

        patch_applied_at(&path, "corr-001", "2024-01-01T00:00:00Z").unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(
            updated.contains("applied_at: \"2024-01-01T00:00:00Z\""),
            "applied_at not found in: {updated}"
        );
        // Original content preserved
        assert!(updated.contains("edge_uuid: abc"));
    }

    #[test]
    fn test_patch_applied_at_updates_existing() {
        let dir = TempDir::new().unwrap();
        let content =
            "corrections:\n  - id: corr-001\n    type: retract\n    applied_at: \"2023-01-01T00:00:00Z\"\n    edge_uuid: abc\n";
        let path = dir.path().join("corrections.yaml");
        std::fs::write(&path, content).unwrap();

        patch_applied_at(&path, "corr-001", "2024-06-01T00:00:00Z").unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("applied_at: \"2024-06-01T00:00:00Z\""));
        assert!(!updated.contains("2023-01-01"));
    }

    #[test]
    fn test_patch_applied_at_preserves_other_corrections() {
        let dir = TempDir::new().unwrap();
        let content = "corrections:\n  - id: corr-001\n    type: retract\n    edge_uuid: abc\n  - id: corr-002\n    type: retract\n    edge_uuid: def\n";
        let path = dir.path().join("corrections.yaml");
        std::fs::write(&path, content).unwrap();

        patch_applied_at(&path, "corr-001", "2024-01-01T00:00:00Z").unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("corr-002"), "second correction was lost");
        assert!(updated.contains("def"), "second edge_uuid was lost");
    }

    #[test]
    fn test_corrections_file_path() {
        let path = corrections_file_path(Path::new("/workspace"));
        assert_eq!(
            path,
            PathBuf::from("/workspace/.liminis/knowledge-corrections.yaml")
        );
    }

    // ── find_leaf_type ─────────────────────────────────────────────────────────

    fn make_ancestor_map(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, vs)| (k.to_string(), vs.iter().map(|v| v.to_string()).collect()))
            .collect()
    }

    fn labels(vs: &[&str]) -> Vec<String> {
        vs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_leaf_type_single_label_entity_returns_none() {
        let map = HashMap::new();
        assert_eq!(find_leaf_type(&labels(&["Entity"]), &map), None);
    }

    #[test]
    fn find_leaf_type_flat_typed_entity_returns_leaf() {
        let map = make_ancestor_map(&[("Person", &[])]);
        assert_eq!(
            find_leaf_type(&labels(&["Entity", "Person"]), &map),
            Some("Person".to_string())
        );
    }

    #[test]
    fn find_leaf_type_multi_label_ancestor_returns_leaf() {
        // RFC is the leaf; Document is its ancestor
        let map = make_ancestor_map(&[("Document", &[]), ("Rfc", &["Document"])]);
        assert_eq!(
            find_leaf_type(&labels(&["Entity", "Document", "Rfc"]), &map),
            Some("Rfc".to_string())
        );
    }

    #[test]
    fn find_leaf_type_undeclared_type_returns_none() {
        // Council is not in the ancestor_map (flat ontology with no declarations)
        let map = HashMap::new();
        assert_eq!(find_leaf_type(&labels(&["Entity", "Council"]), &map), None);
    }
}
