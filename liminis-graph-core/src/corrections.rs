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

// ── File location ─────────────────────────────────────────────────────────────

pub fn corrections_file_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".liminis").join("knowledge-corrections.yaml")
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
    let file: CorrectionsFile = serde_yaml::from_str(&text)
        .map_err(|e| Error::Ipc(format!("YAML parse error: {e}")))?;
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

    // Find the line containing this correction's id
    // Match both inline (`- id: <val>`) and standalone (`id: <val>`) YAML forms.
    let id_bare = format!("id: {id}");
    let id_quoted = format!("id: \"{id}\"");
    let id_inline_bare = format!("- id: {id}");
    let id_inline_quoted = format!("- id: \"{id}\"");
    let id_line_idx = lines
        .iter()
        .position(|l| {
            let t = l.trim();
            t == id_bare || t == id_quoted || t == id_inline_bare || t == id_inline_quoted
        })
        .ok_or_else(|| Error::Ipc(format!("correction id '{id}' not found in corrections file")))?;

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

    // Check if applied_at already exists in the block
    let applied_at_idx = (id_line_idx..block_end)
        .find(|&i| lines[i].trim_start().starts_with("applied_at:"));

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
            dfs_detect(start, &alias_to_canonical, &mut state, &mut path, &mut cycles);
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
                let cycle_nodes: Vec<&str> = path[cycle_start..].iter().map(|s| s.as_str()).collect();
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
    let unapplied = file.corrections.iter().filter(|e| e.applied_at.is_none()).count();
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

pub fn apply_corrections_file(
    conn: &Conn,
    workspace_root: &Path,
    dry_run: bool,
) -> ApplyResult {
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
        return Err(Error::Ipc("same_as requires non-empty 'aliases'".to_string()));
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

                // De-duplicate: skip if canonical already has a directed edge to same endpoint
                if conn.has_directed_edge(&new_src, &new_dst)? {
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

    Ok(if dry_run { "dry_run:same_as" } else { "same_as" }.to_string())
}

fn apply_retract(
    conn: &Conn,
    entry: &CorrectionEntry,
    path: &Path,
    ts: &str,
    dry_run: bool,
) -> Result<String, Error> {
    let edge_uuid = entry.edge_uuid.as_deref().ok_or_else(|| {
        Error::Ipc("retract requires 'edge_uuid'".to_string())
    })?;

    // Verify edge exists (same check as validate_corrections per FR-015 dry_run requirement)
    conn.get_edge_by_uuid(edge_uuid)?
        .ok_or_else(|| Error::Ipc(format!("edge_uuid '{edge_uuid}' not found in graph")))?;

    if !dry_run {
        conn.invalidate_edge(edge_uuid, ts)?;
        patch_applied_at(path, &entry.id, ts)?;
    }

    Ok(if dry_run { "dry_run:retract" } else { "retract" }.to_string())
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
/// Returns the number of entities actually updated.
pub fn apply_entity_type_labels(
    conn: &Conn,
    updates: &[(String, String)],
) -> Result<usize, Error> {
    let mut count = 0;
    for (uuid, entity_type) in updates {
        if entity_type.is_empty() {
            continue;
        }
        let labels = vec!["Entity".to_string(), entity_type.clone()];
        conn.update_entity_labels(uuid, &labels)?;
        count += 1;
    }
    Ok(count)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

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
        assert!(err.contains("YAML parse error"), "expected parse error, got: {err}");
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
}
