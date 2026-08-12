//! Resolvable semantic pointers for cross-graph references (issue #369).
//!
//! A cross-group `RelatesToEdge` carries, for each endpoint foreign to the edge's own
//! `group_id`, a [`CrossGroupPointer`] additively nested under the `cross_group_pointers` key
//! of the `RelatesToNode_` shadow node's existing `attributes` JSON column — no schema
//! migration. The UUID FK (`source_node_uuid`/`target_node_uuid`) is demoted from identity to
//! resolution cache for a foreign endpoint: the pointer's `source_group_id` + `endpoint_name`
//! are the durable assertion ("which graph, what name"), while `resolved_uuid`, `bound_at_seq`,
//! and `binding_state` are what the last resolution pass produced.
//!
//! See `docs/adr/0369-resolvable-cross-group-pointers.md`.

use serde::{Deserialize, Serialize};

/// Tri-state resolution outcome for a cross-group pointer, orthogonal to `invalid_at`
/// (temporal validity — "this fact stopped being true"). `Unbound` means "no current match in
/// the source group" (likely mid-rebuild or renamed), not "the source retracted this."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingState {
    /// Resolves to exactly one entity.
    Bound,
    /// No current match.
    Unbound,
    /// More than one candidate currently matches.
    Ambiguous,
}

/// Which endpoint of a `RelatesToEdge` a pointer describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointSide {
    Src,
    Dst,
}

/// One resolvable semantic pointer for a single foreign endpoint.
///
/// `source_group_id` and `endpoint_name` are the assertion; `resolved_uuid`, `bound_at_seq`,
/// and `binding_state` are the cache produced by the last resolution/re-bind pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossGroupPointer {
    /// Which group the endpoint entity lives in.
    pub source_group_id: String,
    /// Normalized name of the endpoint entity in that group.
    pub endpoint_name: String,
    /// UUID last resolved to, when `binding_state == Bound`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_uuid: Option<String>,
    /// Source's `WalPosition.applied_seq` at the time of the last resolution (FR-007's
    /// staleness gate; per-database singleton until #360 adds per-source positions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_at_seq: Option<u64>,
    pub binding_state: BindingState,
}

/// Pointer fields for a cross-group edge's foreign endpoints. An edge with both endpoints in
/// its own `group_id` has neither field set and is never given this key at all (FR-001) — see
/// [`write_pointers`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CrossGroupPointers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<CrossGroupPointer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<CrossGroupPointer>,
}

impl CrossGroupPointers {
    pub fn is_empty(&self) -> bool {
        self.src.is_none() && self.dst.is_none()
    }

    pub fn get(&self, side: EndpointSide) -> Option<&CrossGroupPointer> {
        match side {
            EndpointSide::Src => self.src.as_ref(),
            EndpointSide::Dst => self.dst.as_ref(),
        }
    }

    pub fn set(&mut self, side: EndpointSide, pointer: CrossGroupPointer) {
        match side {
            EndpointSide::Src => self.src = Some(pointer),
            EndpointSide::Dst => self.dst = Some(pointer),
        }
    }

    /// Iterates over whichever of `(Src, ptr)` / `(Dst, ptr)` are present.
    pub fn iter(&self) -> impl Iterator<Item = (EndpointSide, &CrossGroupPointer)> {
        self.src
            .iter()
            .map(|p| (EndpointSide::Src, p))
            .chain(self.dst.iter().map(|p| (EndpointSide::Dst, p)))
    }
}

const POINTERS_KEY: &str = "cross_group_pointers";

/// Parses the `cross_group_pointers` object out of a `RelatesToNode_.attributes` JSON string.
/// Absent key, non-object `attributes`, or a malformed `cross_group_pointers` value all yield
/// the empty (no foreign endpoints) case — pointer parsing must never break reads of the
/// overwhelming majority of edges that don't carry this key.
pub fn read_pointers(attributes: &str) -> CrossGroupPointers {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(attributes)
    else {
        return CrossGroupPointers::default();
    };
    match map.get(POINTERS_KEY) {
        Some(raw) => serde_json::from_value(raw.clone()).unwrap_or_default(),
        None => CrossGroupPointers::default(),
    }
}

/// Merges `pointers` into `attributes`'s `cross_group_pointers` key, preserving every other
/// existing key untouched. An empty `pointers` removes the key entirely rather than writing
/// `{}`, so an intra-group edge never carries a spurious empty pointer object (FR-001).
/// Non-object or malformed `attributes` is treated as an empty object rather than propagating
/// an error — `attributes` is free-form JSON manipulated the same way at every other call site
/// in this codebase (e.g. `episode.rs`).
pub fn write_pointers(attributes: &str, pointers: &CrossGroupPointers) -> String {
    let mut map = match serde_json::from_str::<serde_json::Value>(attributes) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if pointers.is_empty() {
        map.remove(POINTERS_KEY);
    } else {
        map.insert(
            POINTERS_KEY.to_string(),
            serde_json::to_value(pointers).unwrap_or(serde_json::Value::Null),
        );
    }
    serde_json::Value::Object(map).to_string()
}

const MERGED_INTO_KEY: &str = "merged_into";

/// Reads the `merged_into` forwarding reference recorded on a tombstoned (`Merged`-labelled)
/// entity's `attributes` JSON, if any (issue #371, FR-005/FR-007). Absent key, non-object
/// `attributes`, or a non-string value all yield `None` — a missing forwarding reference is the
/// expected, permanent shape for every alias tombstoned before this feature shipped, not an
/// error.
pub fn read_merged_into(attributes: &str) -> Option<String> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(attributes)
    else {
        return None;
    };
    map.get(MERGED_INTO_KEY)?.as_str().map(|s| s.to_string())
}

/// Records `canonical_uuid` under the `merged_into` key of `attributes`, preserving every other
/// existing key untouched (mirrors [`write_pointers`]'s additive-attributes idiom). Non-object or
/// malformed `attributes` is treated as an empty object rather than propagating an error.
pub fn write_merged_into(attributes: &str, canonical_uuid: &str) -> String {
    let mut map = match serde_json::from_str::<serde_json::Value>(attributes) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    map.insert(
        MERGED_INTO_KEY.to_string(),
        serde_json::Value::String(canonical_uuid.to_string()),
    );
    serde_json::Value::Object(map).to_string()
}

/// Counts of cross-group pointers by [`BindingState`], for `knowledge_status` (FR-012).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerStateCounts {
    pub bound: u64,
    pub unbound: u64,
    pub ambiguous: u64,
}

impl PointerStateCounts {
    pub fn record(&mut self, state: BindingState) {
        match state {
            BindingState::Bound => self.bound += 1,
            BindingState::Unbound => self.unbound += 1,
            BindingState::Ambiguous => self.ambiguous += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pointer(name: &str) -> CrossGroupPointer {
        CrossGroupPointer {
            source_group_id: "source-a".to_string(),
            endpoint_name: name.to_string(),
            resolved_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            bound_at_seq: Some(42),
            binding_state: BindingState::Bound,
        }
    }

    #[test]
    fn absent_key_is_empty() {
        let pointers = read_pointers(r#"{"foo": "bar"}"#);
        assert!(pointers.is_empty());
        assert_eq!(pointers.get(EndpointSide::Src), None);
        assert_eq!(pointers.get(EndpointSide::Dst), None);
    }

    #[test]
    fn absent_or_malformed_attributes_is_empty() {
        assert!(read_pointers("").is_empty());
        assert!(read_pointers("not json").is_empty());
        assert!(read_pointers("[]").is_empty());
    }

    #[test]
    fn round_trips_a_single_side() {
        let mut pointers = CrossGroupPointers::default();
        pointers.set(EndpointSide::Dst, sample_pointer("alice"));

        let attrs = write_pointers("{}", &pointers);
        let parsed = read_pointers(&attrs);

        assert_eq!(parsed.get(EndpointSide::Src), None);
        assert_eq!(
            parsed.get(EndpointSide::Dst),
            Some(&sample_pointer("alice"))
        );
    }

    #[test]
    fn round_trips_both_sides() {
        let mut pointers = CrossGroupPointers::default();
        pointers.set(EndpointSide::Src, sample_pointer("alice"));
        pointers.set(EndpointSide::Dst, sample_pointer("bob"));

        let attrs = write_pointers("{}", &pointers);
        let parsed = read_pointers(&attrs);

        assert_eq!(
            parsed.get(EndpointSide::Src),
            Some(&sample_pointer("alice"))
        );
        assert_eq!(parsed.get(EndpointSide::Dst), Some(&sample_pointer("bob")));
    }

    #[test]
    fn preserves_unrelated_existing_keys() {
        let mut pointers = CrossGroupPointers::default();
        pointers.set(EndpointSide::Src, sample_pointer("alice"));

        let attrs = write_pointers(r#"{"custom_field": "value", "n": 7}"#, &pointers);
        let value: serde_json::Value = serde_json::from_str(&attrs).unwrap();

        assert_eq!(value["custom_field"], "value");
        assert_eq!(value["n"], 7);
        assert!(value.get("cross_group_pointers").is_some());
    }

    #[test]
    fn empty_pointers_removes_key_rather_than_writing_empty_object() {
        let attrs = write_pointers(
            r#"{"cross_group_pointers": {"src": null, "dst": null}, "kept": true}"#,
            &CrossGroupPointers::default(),
        );
        let value: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert!(value.get("cross_group_pointers").is_none());
        assert_eq!(value["kept"], true);
    }

    #[test]
    fn unbound_pointer_has_no_resolved_uuid() {
        let ptr = CrossGroupPointer {
            source_group_id: "source-a".to_string(),
            endpoint_name: "carol".to_string(),
            resolved_uuid: None,
            bound_at_seq: None,
            binding_state: BindingState::Unbound,
        };
        let mut pointers = CrossGroupPointers::default();
        pointers.set(EndpointSide::Src, ptr.clone());
        let attrs = write_pointers("{}", &pointers);
        let parsed = read_pointers(&attrs);
        assert_eq!(parsed.get(EndpointSide::Src), Some(&ptr));
    }

    #[test]
    fn merged_into_absent_by_default() {
        assert_eq!(read_merged_into("{}"), None);
        assert_eq!(read_merged_into(""), None);
        assert_eq!(read_merged_into("not json"), None);
        assert_eq!(read_merged_into("[]"), None);
    }

    #[test]
    fn merged_into_round_trips() {
        let attrs = write_merged_into("{}", "canonical-uuid-1");
        assert_eq!(
            read_merged_into(&attrs),
            Some("canonical-uuid-1".to_string())
        );
    }

    #[test]
    fn merged_into_preserves_unrelated_existing_keys() {
        let attrs = write_merged_into(r#"{"custom_field": "value"}"#, "canonical-uuid-1");
        let value: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(value["custom_field"], "value");
        assert_eq!(value["merged_into"], "canonical-uuid-1");
    }

    #[test]
    fn state_counts_record_correctly() {
        let mut counts = PointerStateCounts::default();
        counts.record(BindingState::Bound);
        counts.record(BindingState::Bound);
        counts.record(BindingState::Unbound);
        counts.record(BindingState::Ambiguous);
        assert_eq!(
            counts,
            PointerStateCounts {
                bound: 2,
                unbound: 1,
                ambiguous: 1,
            }
        );
    }
}
