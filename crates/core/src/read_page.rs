//! Paging cursor, field projection and optional-parameter parsing shared by the bulk read
//! handlers `knowledge_get_episodes` and `knowledge_list_entities` (issue #667, ADR-0667).
//!
//! A cursor is the lowercase-hex encoding of a JSON payload
//! `{"v":1,"t":"<tool tag>","shape":"<sha256 hex>","pos":{...}}`. `pos` is a keyset position
//! (stable under concurrent inserts and deletes, unlike an offset over a newest-first order);
//! `shape` hashes every input that changes result membership or order, so a cursor replayed
//! against a different query is rejected rather than silently paging a different result set.
//! `fields` is deliberately excluded from the shape: projection never changes membership.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::Error;

/// Serialized keys of an episode item (embeddings are `#[serde(skip)]`, never projectable).
pub const EPISODE_FIELDS: &[&str] = &[
    "uuid",
    "name",
    "group_id",
    "created_at",
    "ingested_at",
    "source",
    "source_description",
    "content",
    "valid_at",
    "entity_edges",
    "attributes",
];

/// Serialized keys of an entity item (embeddings are `#[serde(skip)]`, never projectable).
pub const ENTITY_FIELDS: &[&str] = &[
    "uuid",
    "name",
    "group_id",
    "labels",
    "kind",
    "created_at",
    "ingested_at",
    "summary",
    "attributes",
    "episode_uuids",
    "source_descriptions",
];

pub const EPISODES_TAG: &str = "episodes";
pub const ENTITIES_TAG: &str = "entities";

const CURSOR_VERSION: u64 = 1;

/// Parsed optional read-path parameters.
#[derive(Debug, Default, PartialEq)]
pub struct ReadOpts {
    /// Raw (still-encoded) cursor; `None` for an absent, null or empty cursor (first page).
    pub cursor: Option<String>,
    /// Validated projection; `None` returns the full shape.
    pub fields: Option<Vec<String>>,
    /// Non-empty name prefix; `None` means no filter.
    pub name_prefix: Option<String>,
    /// True when any of `cursor` / `fields` / `name_prefix` was present in the request (a JSON
    /// `null` counts). Only then does the response carry `next_cursor` (FR-017 byte-identity).
    pub paged: bool,
}

fn has_key(p: &Value, key: &str) -> bool {
    p.as_object().is_some_and(|o| o.contains_key(key))
}

/// Parses `cursor`, `fields` and `name_prefix` out of the request params, validating `fields`
/// against `allowed`. Errors are `Error::Ipc` (the same `-32000` as other validation errors).
pub fn parse_read_opts(p: &Value, allowed: &[&str]) -> Result<ReadOpts, Error> {
    let paged = has_key(p, "cursor") || has_key(p, "fields") || has_key(p, "name_prefix");

    let cursor = match &p["cursor"] {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        _ => return Err(Error::Ipc("cursor must be a string".to_string())),
    };

    let name_prefix = match &p["name_prefix"] {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        _ => return Err(Error::Ipc("name_prefix must be a string".to_string())),
    };

    let fields = match &p["fields"] {
        Value::Null => None,
        Value::Array(arr) => {
            if arr.is_empty() {
                return Err(Error::Ipc("fields must not be empty".to_string()));
            }
            let mut out: Vec<String> = Vec::with_capacity(arr.len());
            for f in arr {
                let name = f
                    .as_str()
                    .ok_or_else(|| Error::Ipc("fields must be an array of strings".to_string()))?;
                if !allowed.contains(&name) {
                    return Err(Error::Ipc(format!(
                        "unknown field '{name}' (allowed: {})",
                        allowed.join(", ")
                    )));
                }
                if !out.iter().any(|o| o == name) {
                    out.push(name.to_string());
                }
            }
            Some(out)
        }
        _ => return Err(Error::Ipc("fields must be an array of strings".to_string())),
    };

    Ok(ReadOpts {
        cursor,
        fields,
        name_prefix,
        paged,
    })
}

/// Keeps only `fields` (in the requested order) from a serialized item.
pub fn project(item: Value, fields: &[String]) -> Value {
    let mut out = serde_json::Map::with_capacity(fields.len());
    for f in fields {
        if let Some(v) = item.get(f.as_str()) {
            out.insert(f.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Hashes every input that affects result membership or order. `groups` is sorted and
/// deduplicated so equivalent group sets hash identically; `None` (all groups) is distinct from
/// every explicit set.
pub fn shape_hash(
    tag: &str,
    groups: Option<&[String]>,
    kind: Option<&str>,
    name_prefix: Option<&str>,
) -> String {
    let groups = groups.map(|g| {
        let mut g: Vec<&str> = g.iter().map(String::as_str).collect();
        g.sort_unstable();
        g.dedup();
        g
    });
    let canonical = json!([tag, groups, kind, name_prefix]).to_string();
    hex_encode(&Sha256::digest(canonical.as_bytes()))
}

pub fn encode_cursor(tag: &str, shape: &str, pos: Value) -> String {
    let payload = json!({ "v": CURSOR_VERSION, "t": tag, "shape": shape, "pos": pos });
    hex_encode(payload.to_string().as_bytes())
}

/// Decodes a cursor, checking version, tool tag and query shape. Returns the keyset position.
pub fn decode_cursor(cursor: &str, tag: &str, shape: &str) -> Result<Value, Error> {
    let bad = |why: &str| Error::Ipc(format!("invalid cursor: {why}"));
    let bytes = hex_decode(cursor).ok_or_else(|| bad("malformed"))?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| bad("malformed"))?;
    if payload["v"].as_u64() != Some(CURSOR_VERSION) {
        return Err(bad("unsupported version"));
    }
    if payload["t"].as_str() != Some(tag) || payload["shape"].as_str() != Some(shape) {
        return Err(bad("cursor does not belong to this query"));
    }
    match payload.get("pos") {
        Some(pos) if pos.is_object() => Ok(pos.clone()),
        _ => Err(bad("malformed")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(p: Value) -> Result<ReadOpts, Error> {
        parse_read_opts(&p, EPISODE_FIELDS)
    }

    #[test]
    fn no_params_is_not_paged() {
        let o = opts(json!({"last_n": 5})).unwrap();
        assert_eq!(o, ReadOpts::default());
        assert!(!o.paged);
    }

    #[test]
    fn any_paging_param_marks_paged() {
        assert!(opts(json!({"cursor": ""})).unwrap().paged);
        assert!(opts(json!({"cursor": null})).unwrap().paged);
        assert!(opts(json!({"fields": ["uuid"]})).unwrap().paged);
        assert!(opts(json!({"name_prefix": ""})).unwrap().paged);
    }

    #[test]
    fn empty_cursor_and_prefix_mean_absent() {
        let o = opts(json!({"cursor": "", "name_prefix": ""})).unwrap();
        assert_eq!(o.cursor, None);
        assert_eq!(o.name_prefix, None);
    }

    #[test]
    fn fields_validation() {
        assert!(opts(json!({"fields": []})).is_err());
        assert!(opts(json!({"fields": "uuid"})).is_err());
        assert!(opts(json!({"fields": [1]})).is_err());
        let e = opts(json!({"fields": ["uuid", "bogus"]})).unwrap_err();
        assert!(e.to_string().contains("bogus"), "{e}");
        // Embeddings are never projectable.
        assert!(opts(json!({"fields": ["content_embedding"]})).is_err());
        let o = opts(json!({"fields": ["name", "uuid", "name"]})).unwrap();
        assert_eq!(o.fields, Some(vec!["name".to_string(), "uuid".to_string()]));
    }

    #[test]
    fn non_string_cursor_and_prefix_rejected() {
        assert!(opts(json!({"cursor": 3})).is_err());
        assert!(opts(json!({"name_prefix": ["a"]})).is_err());
    }

    #[test]
    fn project_keeps_only_requested_keys() {
        let item = json!({"uuid": "u", "name": "n", "content": "big"});
        let out = project(item, &["name".to_string(), "uuid".to_string()]);
        assert_eq!(out, json!({"name": "n", "uuid": "u"}));
    }

    #[test]
    fn cursor_round_trip() {
        let shape = shape_hash(EPISODES_TAG, Some(&["g".to_string()]), None, None);
        let c = encode_cursor(EPISODES_TAG, &shape, json!({"uuid": "x"}));
        let pos = decode_cursor(&c, EPISODES_TAG, &shape).unwrap();
        assert_eq!(pos, json!({"uuid": "x"}));
    }

    #[test]
    fn cursor_rejects_tamper_and_wrong_shape() {
        let shape = shape_hash(ENTITIES_TAG, None, None, None);
        let c = encode_cursor(ENTITIES_TAG, &shape, json!({"uuid": "x"}));
        assert!(decode_cursor("zz", ENTITIES_TAG, &shape).is_err());
        assert!(decode_cursor(&c[..c.len() - 1], ENTITIES_TAG, &shape).is_err());
        assert!(decode_cursor(&hex_encode(b"not json"), ENTITIES_TAG, &shape).is_err());
        // Different tool tag.
        assert!(decode_cursor(&c, EPISODES_TAG, &shape).is_err());
        // Different groups / kind / prefix.
        for other in [
            shape_hash(ENTITIES_TAG, Some(&["g".to_string()]), None, None),
            shape_hash(ENTITIES_TAG, None, Some("Person"), None),
            shape_hash(ENTITIES_TAG, None, None, Some("al")),
        ] {
            assert!(decode_cursor(&c, ENTITIES_TAG, &other).is_err());
        }
    }

    #[test]
    fn shape_hash_ignores_group_order_and_duplicates() {
        let a = shape_hash(
            EPISODES_TAG,
            Some(&["b".to_string(), "a".to_string(), "a".to_string()]),
            None,
            None,
        );
        let b = shape_hash(
            EPISODES_TAG,
            Some(&["a".to_string(), "b".to_string()]),
            None,
            None,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn hex_round_trip_non_ascii() {
        let s = "héllo";
        assert_eq!(hex_decode(&hex_encode(s.as_bytes())).unwrap(), s.as_bytes());
        assert!(hex_decode("é0").is_none());
    }
}
