//! Legacy-WAL translation layer: Cypher-text and param-shape transforms.
//!
//! This module handles FalkorDB-dialect Cypher constructs that lbug cannot execute directly.
//! It operates on the raw Cypher string and/or the shape of the params object — before
//! `interpolate_params` converts param values to Cypher literals.
//!
//! **Module split rule** (follow this when adding future legacy-compat fixes):
//! - Transforms that rewrite the Cypher string or reshape the params map → this module
//! - Transforms that change only how a param *value* is formatted as a Cypher literal →
//!   `replay.rs::json_to_cypher_literal` (e.g., string escaping, RFC-3339 timestamp literals)
//!
//! Pipeline order in `replay.rs`: `strip_vecf32` → `expand_bulk_property_set` → `interpolate_params`

use std::sync::OnceLock;

use regex::Regex;

/// Strips FalkorDB's `vecf32(...)` vector-constructor wrapper from a Cypher string.
///
/// lbug's `FLOAT[N]` columns accept a bare list literal or list-typed param directly;
/// the `vecf32(...)` wrapper is FalkorDB-only and causes `Catalog exception: function VECF32
/// does not exist`. Stripping it is lossless.
///
/// Handles all case variants (`vecf32`, `VECF32`, `VecF32`, …), both inline-array form
/// `vecf32([0.1, 0.2])` and param-ref form `vecf32($emb)` / `vecf32($a.b)`, and multiple
/// occurrences in one statement. Uses a balanced-parenthesis scan to find the matching close
/// paren — square brackets inside the vector literal do not affect paren depth and are safe.
pub(crate) fn strip_vecf32(cypher: &str) -> String {
    let needle = "vecf32(";
    let mut result = String::with_capacity(cypher.len());
    let mut rest = cypher;

    loop {
        // Case-insensitive search: find the next `vecf32(` in `rest`.
        // `to_ascii_lowercase()` only lowercases ASCII bytes and leaves non-ASCII bytes
        // unchanged, so byte positions align between `lower` and `rest`.
        let lower = rest.to_ascii_lowercase();
        let Some(rel_pos) = lower.find(needle) else {
            result.push_str(rest);
            break;
        };

        // Emit everything before the match unchanged.
        result.push_str(&rest[..rel_pos]);

        // Scan the content after `vecf32(` for the matching `)`.
        // Track `()` depth only; `[]` inside inline vector literals don't interfere.
        let after_open = &rest[rel_pos + needle.len()..];
        let mut depth = 1usize;
        let mut inner_end = None;
        for (i, ch) in after_open.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        inner_end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }

        if let Some(end) = inner_end {
            // Emit only the inner content — drop `vecf32(` and the closing `)`.
            result.push_str(&after_open[..end]);
            rest = &rest[rel_pos + needle.len() + end + 1..];
        } else {
            // Unmatched paren (shouldn't occur in valid WAL) — emit literally and continue.
            result.push_str(&rest[rel_pos..rel_pos + needle.len()]);
            rest = &rest[rel_pos + needle.len()..];
        }
    }

    result
}

/// Expands FalkorDB/Neo4j bulk property-set `SET n = $props` to individual assignments.
///
/// lbug requires individual `SET n.k = $v` assignments; the bulk `SET n = $props` form is
/// FalkorDB/Neo4j syntax. When `$props` is a JSON object, this rewrites `SET n = $props` to
/// `SET n.k1 = $props_k1, n.k2 = $props_k2, …` and flattens the nested object into top-level
/// params (prefixed with the original param name to avoid collisions with existing params).
///
/// Individual assignments (`SET n.field = $x`) are detected by the dot in the LHS and left
/// unchanged. Returns the (possibly rewritten) Cypher string and the (possibly extended) params.
pub(crate) fn expand_bulk_property_set(
    cypher: &str,
    params: &serde_json::Value,
) -> (String, serde_json::Value) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Matches `SET <var> = $<param>` where <var> contains no dot (bulk form only).
        // The character class `[A-Za-z0-9_]*` stops at a dot, so `SET n.field = $x` produces
        // var="n" but then `\s*=` fails on "." — the whole match is rejected.
        // `\b` after the param name prevents partial matches like `$props` vs `$props_extra`.
        Regex::new(r"(?i)SET\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*\$([A-Za-z_][A-Za-z0-9_]*)\b")
            .expect("valid regex")
    });

    let serde_json::Value::Object(param_map) = params else {
        return (cypher.to_string(), params.clone());
    };

    // Collect matches where the referenced param is a JSON object.
    let matches: Vec<_> = re
        .captures_iter(cypher)
        .filter_map(|cap| {
            let full = cap.get(0)?;
            let var_name = cap.get(1)?.as_str().to_string();
            let param_name = cap.get(2)?.as_str().to_string();
            let param_val = param_map.get(&param_name)?;
            if let serde_json::Value::Object(obj) = param_val {
                Some((full.start(), full.end(), var_name, param_name, obj.clone()))
            } else {
                None
            }
        })
        .collect();

    if matches.is_empty() {
        return (cypher.to_string(), params.clone());
    }

    let mut new_params = serde_json::Value::Object(param_map.clone());
    let param_obj = new_params.as_object_mut().unwrap();

    // Build per-match replacements and extend the params map with flattened keys.
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    for (start, end, var_name, param_name, obj) in &matches {
        let mut assignments: Vec<String> = Vec::new();
        for (key, value) in obj {
            let flat_key = format!("{param_name}_{key}");
            assignments.push(format!("{var_name}.{key} = ${flat_key}"));
            param_obj.insert(flat_key, value.clone());
        }
        replacements.push((*start, *end, format!("SET {}", assignments.join(", "))));
    }

    // Apply right-to-left so earlier byte offsets remain valid after each splice.
    replacements.sort_by_key(|(start, _, _)| *start);
    replacements.reverse();

    let mut result = cypher.to_string();
    for (start, end, replacement) in replacements {
        result.replace_range(start..end, &replacement);
    }

    (result, new_params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- strip_vecf32 ---

    #[test]
    fn strip_vecf32_uppercase_inline() {
        assert_eq!(
            strip_vecf32("SET n.emb = VECF32([0.1, -0.2, 0.3])"),
            "SET n.emb = [0.1, -0.2, 0.3]"
        );
    }

    #[test]
    fn strip_vecf32_lowercase_param_ref() {
        assert_eq!(strip_vecf32("SET n.emb = vecf32($emb)"), "SET n.emb = $emb");
    }

    #[test]
    fn strip_vecf32_mixedcase_dotted_param() {
        assert_eq!(
            strip_vecf32("SET n.emb = VecF32($p.embedding)"),
            "SET n.emb = $p.embedding"
        );
    }

    #[test]
    fn strip_vecf32_multiple_occurrences() {
        assert_eq!(
            strip_vecf32("SET n.a = VECF32([1.0, 0.0]), n.b = vecf32($b_emb)"),
            "SET n.a = [1.0, 0.0], n.b = $b_emb"
        );
    }

    #[test]
    fn strip_vecf32_no_op_when_absent() {
        let cypher = "MERGE (n:Entity {uuid: $uuid}) SET n.name = $name";
        assert_eq!(strip_vecf32(cypher), cypher);
    }

    // --- expand_bulk_property_set ---

    #[test]
    fn expand_bulk_set_expands_object_param() {
        let cypher = "MERGE (n:Entity {uuid: $uuid}) SET n = $props";
        let params = json!({"uuid": "abc", "props": {"name": "Alice", "group_id": "g1"}});
        let (new_cypher, new_params) = expand_bulk_property_set(cypher, &params);
        assert!(
            new_cypher.contains("n.name = $props_name"),
            "expected individual assignment; got: {new_cypher}"
        );
        assert!(
            new_cypher.contains("n.group_id = $props_group_id"),
            "expected individual assignment; got: {new_cypher}"
        );
        assert_eq!(new_params["props_name"], json!("Alice"));
        assert_eq!(new_params["props_group_id"], json!("g1"));
    }

    #[test]
    fn expand_bulk_set_leaves_individual_assignments_unchanged() {
        let cypher = "MERGE (n:Entity {uuid: $uuid}) SET n.name = $name";
        let params = json!({"uuid": "abc", "name": "Alice"});
        let (new_cypher, _) = expand_bulk_property_set(cypher, &params);
        assert_eq!(new_cypher, cypher);
    }

    #[test]
    fn expand_bulk_set_mixed_bulk_and_individual() {
        let cypher = "MERGE (n:Entity {uuid: $uuid}) SET n = $props, n.extra = $extra";
        let params = json!({"uuid": "abc", "props": {"name": "Alice"}, "extra": "val"});
        let (new_cypher, new_params) = expand_bulk_property_set(cypher, &params);
        assert!(
            new_cypher.contains("n.name = $props_name"),
            "bulk expansion missing; got: {new_cypher}"
        );
        assert!(
            new_cypher.contains("n.extra = $extra"),
            "individual assignment should survive; got: {new_cypher}"
        );
        assert_eq!(new_params["props_name"], json!("Alice"));
    }

    #[test]
    fn expand_bulk_set_key_prefix_avoids_collision() {
        // Top-level $uuid must not be clobbered by $props.uuid being flattened to $uuid.
        let cypher = "MERGE (n:Entity {uuid: $uuid}) SET n = $props";
        let params = json!({
            "uuid": "top-level-uuid",
            "props": {"uuid": "obj-uuid", "name": "Bob"}
        });
        let (new_cypher, new_params) = expand_bulk_property_set(cypher, &params);
        assert!(
            new_cypher.contains("n.uuid = $props_uuid"),
            "flattened key should be prefixed; got: {new_cypher}"
        );
        assert_eq!(new_params["props_uuid"], json!("obj-uuid"));
        assert_eq!(
            new_params["uuid"],
            json!("top-level-uuid"),
            "original top-level param must not be overwritten"
        );
    }

    #[test]
    fn expand_bulk_set_non_object_param_unchanged() {
        let cypher = "SET n = $props";
        let params = json!({"props": "not-an-object"});
        let (new_cypher, _) = expand_bulk_property_set(cypher, &params);
        assert_eq!(new_cypher, cypher);
    }
}
